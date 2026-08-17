//! Linux resident-memory accounting for the long-lived GHCi session.
//!
//! The process tree is deliberately the right scope for signals and shutdown: hooks and code
//! evaluated by GHCi can start arbitrary descendants, and ghciwatch must be able to stop all of
//! them. It is *not* the right scope for the memory watchdog, however. Tests launched from GHCi
//! can start `cabal test`, batch GHC compiler processes, GCC, and the linker. Summing their RSS
//! can therefore restart a healthy interactive session.
//!
//! Memory accounting charges exactly two persistent processes: the interactive GHC at the known
//! process-tree position (three edges below ghciwatch, two below the configured command process),
//! and that GHC's immediate Cabal parent. The GHC candidate must have an exact `--interactive`
//! argument and resolve through `/proc/PID/exe` to a GHC executable. Selecting the pair directly,
//! rather than summing a process tree or process group, excludes tests, compilers, linkers, and other
//! descendants spawned by the interactive session.

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
    ghc_executable: bool,
    cabal_executable: bool,
}

/// Resident memory belonging to the persistent interactive GHC and its immediate Cabal parent.
#[derive(Debug)]
pub(super) struct MemoryUsage {
    pub(super) bytes: u64,
    pub(super) command_pid: i32,
    pub(super) cabal_parent: Option<(i32, u64)>,
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
        match (self.cabal_parent, self.interactive_ghc) {
            (Some((cabal_pid, cabal_bytes)), Some((ghc_pid, ghc_bytes))) => format!(
                "Command PID: {} (not counted)\nCabal parent PID: {cabal_pid} ({})\nInteractive GHC PID: {ghc_pid} ({})",
                self.command_pid,
                format_bytes(cabal_bytes),
                format_bytes(ghc_bytes),
            ),
            _ => format!(
                "Command PID: {} (not counted)\nCabal parent PID: not found\nInteractive GHC PID: not found",
                self.command_pid,
            ),
        }
    }
}

/// Read RSS for the interactive GHC and its immediate Cabal parent.
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
        let executable = fs::read_link(entry.path().join("exe")).ok();
        let ghc_executable = executable.as_deref().is_some_and(is_ghc_executable);
        let cabal_executable = executable.as_deref().is_some_and(is_cabal_executable);
        processes.insert(
            pid,
            Process {
                pid,
                parent_pid,
                process_group_id,
                resident_bytes,
                interactive,
                ghc_executable,
                cabal_executable,
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
    let selected = if command_is_current {
        processes
            .values()
            .filter(|ghc| ghc.interactive && ghc.ghc_executable)
            .filter(|ghc| descendant_depth(ghc.pid, command_pid, processes) == Some(2))
            .filter_map(|ghc| {
                let cabal = processes.get(&ghc.parent_pid)?;
                (cabal.cabal_executable
                    && descendant_depth(cabal.pid, command_pid, processes) == Some(1))
                .then_some((cabal, ghc))
            })
            .min_by_key(|(_, ghc)| ghc.pid)
    } else {
        None
    };
    let cabal_parent = selected.map(|(cabal, _)| (cabal.pid, cabal.resident_bytes));
    let interactive_ghc = selected.map(|(_, ghc)| (ghc.pid, ghc.resident_bytes));
    let bytes = selected.map_or(0, |(cabal, ghc)| {
        cabal.resident_bytes.saturating_add(ghc.resident_bytes)
    });
    MemoryUsage {
        bytes,
        command_pid,
        cabal_parent,
        interactive_ghc,
    }
}

fn is_ghc_executable(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == "ghc"
                || name
                    .strip_prefix("ghc-")
                    .is_some_and(|version| !version.is_empty())
        })
}

fn is_cabal_executable(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == "cabal"
                || name == ".cabal-wrapped"
                || name
                    .strip_prefix("cabal-")
                    .is_some_and(|version| !version.is_empty())
        })
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
            bytes: 22_809_096_192,
            command_pid: 1_572_392,
            cabal_parent: Some((1_572_421, 104_857_600)),
            interactive_ghc: Some((1_572_439, 22_704_238_592)),
        };
        assert_eq!(
            usage.details(),
            "Command PID: 1572392 (not counted)\nCabal parent PID: 1572421 (100.00 MiB)\nInteractive GHC PID: 1572439 (21.14 GiB)"
        );
    }

    fn process(pid: i32, parent_pid: i32, interactive: bool, mib: u64) -> Process {
        Process {
            pid,
            parent_pid,
            process_group_id: 10,
            resident_bytes: mib * 1024 * 1024,
            interactive,
            ghc_executable: interactive,
            cabal_executable: !interactive,
        }
    }

    #[test]
    fn counts_only_interactive_ghc_and_its_immediate_parent() {
        let processes = BTreeMap::from([
            (10, process(10, 1, false, 2_000)), // cabal repl; configured command, not charged
            (11, process(11, 10, false, 20)),   // .cabal-wrapped act-as-setup; charged
            (12, process(12, 11, true, 12_000)), // ghc --interactive; charged
            (13, process(13, 12, true, 11_000)), // nested GHCi; not charged
            (14, process(14, 12, false, 9_000)), // GHCi child; not charged
            (20, process(20, 10, true, 4_000)), // other GHCi; too shallow
        ]);
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 12_020 * 1024 * 1024);
        assert_eq!(usage.cabal_parent, Some((11, 20 * 1024 * 1024)));
        assert_eq!(usage.interactive_ghc, Some((12, 12_000 * 1024 * 1024)));
    }

    #[test]
    fn rejects_a_non_ghc_executable_at_the_expected_depth() {
        let mut candidate = process(12, 11, true, 12_000);
        candidate.ghc_executable = false;
        let processes = BTreeMap::from([
            (10, process(10, 1, false, 100)),
            (11, process(11, 10, false, 20)),
            (12, candidate),
        ]);
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 0);
        assert_eq!(usage.interactive_ghc, None);
        assert_eq!(usage.cabal_parent, None);
    }

    #[test]
    fn rejects_a_non_cabal_immediate_parent() {
        let mut parent = process(11, 10, false, 20);
        parent.cabal_executable = false;
        let processes = BTreeMap::from([
            (10, process(10, 1, false, 100)),
            (11, parent),
            (12, process(12, 11, true, 12_000)),
        ]);
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 0);
        assert_eq!(usage.interactive_ghc, None);
        assert_eq!(usage.cabal_parent, None);
    }

    #[test]
    fn finds_the_expected_ghc_in_another_process_group() {
        let mut processes = BTreeMap::from([
            (10, process(10, 1, false, 100)),
            (11, process(11, 10, false, 20)),
            (12, process(12, 11, true, 12_000)),
        ]);
        processes.get_mut(&12).unwrap().process_group_id = 12;
        let usage = select_repl_processes(10, 10, &processes);
        assert_eq!(usage.bytes, 12_020 * 1024 * 1024);
        assert_eq!(usage.cabal_parent, Some((11, 20 * 1024 * 1024)));
        assert_eq!(usage.interactive_ghc, Some((12, 12_000 * 1024 * 1024)));
    }

    #[test]
    fn recognizes_versioned_ghc_executables() {
        assert!(is_ghc_executable(std::path::Path::new(
            "/nix/store/x/bin/ghc"
        )));
        assert!(is_ghc_executable(std::path::Path::new(
            "/nix/store/x/bin/ghc-9.14.1"
        )));
        assert!(!is_ghc_executable(std::path::Path::new(
            "/nix/store/x/bin/ghciwatch"
        )));
    }

    #[test]
    fn recognizes_cabal_executables() {
        assert!(is_cabal_executable(std::path::Path::new(
            "/nix/store/x/bin/cabal"
        )));
        assert!(is_cabal_executable(std::path::Path::new(
            "/nix/store/x/bin/.cabal-wrapped"
        )));
        assert!(is_cabal_executable(std::path::Path::new(
            "/nix/store/x/bin/cabal-3.16.1.0"
        )));
        assert!(!is_cabal_executable(std::path::Path::new(
            "/nix/store/x/bin/ghc"
        )));
    }

    #[test]
    fn parses_process_group_in_the_procfs_mount_namespace() {
        let status = "Name:\tghci\nPPid:\t17\nNSpgid:\t71\t203\nVmRSS:\t20971521 kB\n";
        assert_eq!(
            parse_process_status(status),
            Some((17, 71, 20_971_521 * 1024))
        );
    }
}
