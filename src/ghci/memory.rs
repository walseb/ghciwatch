//! Linux resident-memory accounting for the long-lived GHCi session.
//!
//! The process tree is deliberately the right scope for signals and shutdown: hooks and code
//! evaluated by GHCi can start arbitrary descendants, and ghciwatch must be able to stop all of
//! them. It is *not* the right scope for the memory watchdog, however. Tests launched from GHCi
//! can start `cabal test`, batch GHC compiler processes, GCC, and the linker. Summing their RSS
//! can therefore restart a healthy interactive session.
//!
//! Memory accounting charges only the persistent GHC process running with an exact
//! `--interactive` argument. It does not charge the configured command wrapper (often `cabal`) or
//! any child of the interactive GHC. The shallowest interactive descendant is the session GHC; a
//! nested GHCi launched by evaluated code is deeper. Ancestry, rather than process-group
//! membership, identifies the session because wrappers may put descendants in another group.

use std::collections::BTreeMap;
use std::fs;
use std::io;

#[derive(Clone, Debug)]
struct Process {
    pid: i32,
    parent_pid: i32,
    process_group_id: i32,
    resident_bytes: u64,
    interactive: bool,
}

/// Resident memory belonging to the persistent interactive GHC process.
#[derive(Debug)]
pub(super) struct MemoryUsage {
    pub(super) bytes: u64,
    pub(super) command_pid: i32,
    pub(super) interactive_ghc: Option<(i32, u64)>,
}

/// Format a byte count using binary units, matching the GiB unit used by the watchdog limit.
pub(super) fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1024_u64.pow(4)),
        ("GiB", 1024_u64.pow(3)),
        ("MiB", 1024_u64.pow(2)),
        ("KiB", 1024),
    ];

    for (unit, divisor) in UNITS {
        if bytes >= divisor {
            return format!("{:.2} {unit}", bytes as f64 / divisor as f64);
        }
    }
    format!("{bytes} bytes")
}

impl MemoryUsage {
    pub(super) fn details(&self) -> String {
        match self.interactive_ghc {
            Some((pid, bytes)) => format!(
                "Command PID: {} (not counted)\nInteractive GHC PID: {pid} ({})",
                self.command_pid,
                format_bytes(bytes),
            ),
            None => format!(
                "Command PID: {} (not counted)\nInteractive GHC PID: not found",
                self.command_pid,
            ),
        }
    }
}

/// Read RSS for the shallowest `--interactive` GHC descended from the direct command.
pub(super) fn repl_resident_memory(
    command_pid: i32,
    process_group_id: i32,
) -> io::Result<MemoryUsage> {
    let processes = process_snapshot()?;
    Ok(select_repl_processes(
        command_pid,
        process_group_id,
        &processes,
    ))
}

fn process_snapshot() -> io::Result<BTreeMap<i32, Process>> {
    let mut processes = BTreeMap::new();
    for entry in fs::read_dir("/proc")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // Processes routinely disappear during this scan. That is expected and should not make a
        // watchdog check fail; the next 30-second check will observe a stable replacement.
        let status = match fs::read(entry.path().join("status")) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let Some((parent_pid, process_group_id, resident_bytes)) =
            parse_process_status(&String::from_utf8_lossy(&status))
        else {
            continue;
        };
        let interactive = fs::read(entry.path().join("cmdline"))
            .map(|cmdline| {
                cmdline
                    .split(|byte| *byte == 0)
                    .any(|argument| argument == b"--interactive")
            })
            .unwrap_or(false);
        processes.insert(
            pid,
            Process {
                pid,
                parent_pid,
                process_group_id,
                resident_bytes,
                interactive,
            },
        );
    }
    Ok(processes)
}

fn select_repl_processes(
    command_pid: i32,
    process_group_id: i32,
    processes: &BTreeMap<i32, Process>,
) -> MemoryUsage {
    // Validate the command identity against the process group captured when it was spawned. The
    // interactive GHC itself need not remain in that group; ancestry is the stronger relationship.
    let command_is_current = processes
        .get(&command_pid)
        .is_some_and(|process| process.process_group_id == process_group_id);
    let interactive_ghc = if command_is_current {
        processes
            .values()
            .filter(|process| process.interactive)
            .filter_map(|process| {
                descendant_depth(process.pid, command_pid, processes)
                    .map(|depth| (depth, process.pid, process.resident_bytes))
            })
            .min_by_key(|(depth, pid, _)| (*depth, *pid))
            .map(|(_, pid, bytes)| (pid, bytes))
    } else {
        None
    };
    let bytes = interactive_ghc.map_or(0, |(_, bytes)| bytes);
    MemoryUsage {
        bytes,
        command_pid,
        interactive_ghc,
    }
}

fn descendant_depth(
    mut pid: i32,
    ancestor_pid: i32,
    processes: &BTreeMap<i32, Process>,
) -> Option<usize> {
    for depth in 0..processes.len() {
        if pid == ancestor_pid {
            return Some(depth);
        }
        let parent_pid = processes.get(&pid)?.parent_pid;
        if parent_pid == pid {
            return None;
        }
        pid = parent_pid;
    }
    None
}

fn parse_process_status(status: &str) -> Option<(i32, i32, u64)> {
    let value = |name| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))?
            .split_whitespace()
            .next()?
            .parse::<i32>()
            .ok()
    };
    let parent_pid = value("PPid:")?;
    // NSpgid starts with the ID in the procfs mount's PID namespace, followed by IDs in
    // successively nested namespaces. The PIDs used to read this procfs are in the first namespace.
    let process_group_id = value("NSpgid:")?;
    let resident_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    Some((
        parent_pid,
        process_group_id,
        resident_kib.saturating_mul(1024),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_memory_sizes_with_binary_units() {
        assert_eq!(format_bytes(1023), "1023 bytes");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1_914_204_160), "1.78 GiB");
        assert_eq!(format_bytes(22_704_238_592), "21.14 GiB");
        assert_eq!(format_bytes(24_618_442_752), "22.93 GiB");
        assert_eq!(format_bytes(30_064_771_072), "28.00 GiB");

        let usage = MemoryUsage {
            bytes: 22_704_238_592,
            command_pid: 1_572_392,
            interactive_ghc: Some((1_572_439, 22_704_238_592)),
        };
        assert_eq!(
            usage.details(),
            "Command PID: 1572392 (not counted)\nInteractive GHC PID: 1572439 (21.14 GiB)"
        );
    }

    fn process(pid: i32, parent_pid: i32, interactive: bool, mib: u64) -> Process {
        Process {
            pid,
            parent_pid,
            process_group_id: 10,
            resident_bytes: mib * 1024 * 1024,
            interactive,
        }
    }

    #[test]
    fn counts_only_main_interactive_ghc() {
        let processes = BTreeMap::from([
            (10, process(10, 1, false, 2_000)), // cabal repl; not charged
            (11, process(11, 10, false, 20)), // cabal setup wrapper
            (12, process(12, 11, true, 12_000)), // persistent GHCi
            (20, process(20, 12, false, 8_000)), // cabal test
            (21, process(21, 20, false, 6_000)), // batch GHC
            (22, process(22, 20, true, 4_000)), // nested ghci launched by a test
        ]);
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 12_000 * 1024 * 1024);
        assert_eq!(usage.interactive_ghc, Some((12, 12_000 * 1024 * 1024)));
    }

    #[test]
    fn counts_a_direct_interactive_ghc() {
        let processes = BTreeMap::from([(10, process(10, 1, true, 12_000))]);
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 12_000 * 1024 * 1024);
        assert_eq!(usage.interactive_ghc, Some((10, 12_000 * 1024 * 1024)));
    }

    #[test]
    fn finds_an_interactive_descendant_in_another_process_group() {
        let mut processes = BTreeMap::from([
            (10, process(10, 1, false, 100)),
            (11, process(11, 10, true, 12_000)),
        ]);
        processes.get_mut(&11).unwrap().process_group_id = 11;
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 12_000 * 1024 * 1024);
        assert_eq!(usage.interactive_ghc, Some((11, 12_000 * 1024 * 1024)));
    }

    #[test]
    fn parses_process_group_in_the_procfs_mount_namespace() {
        let status =
            "Name:\tghci\nPPid:\t17\nNSpgid:\t71\t203\nVmRSS:\t20971521 kB\n";
        assert_eq!(
            parse_process_status(status),
            Some((17, 71, 20_971_521 * 1024))
        );
    }
}
