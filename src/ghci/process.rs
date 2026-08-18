use std::future::Future;
use std::pin::Pin;
use std::process::ExitStatus;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use command_group::AsyncGroupChild;
use eyre::Context;
use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::sync::mpsc;
use tracing::instrument;

use crate::clonable_command::ClonableCommand;
use crate::shutdown::ShutdownHandle;
use tokio::sync::oneshot;

const BEFORE_SIGNAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub struct GhciProcess {
    pub shutdown: ShutdownHandle,
    pub process_group_id: Pid,
    /// PID of the process created directly from `--command`. Unlike the process-group ID, this
    /// anchors descendant discovery even when a child creates a new process group.
    pub process_id: Pid,
    /// Requests intentional shutdown and acknowledges only after the process tree exits. Keeping
    /// the old pipe readers alive until acknowledgement prevents shutdown output from hitting a
    /// closed stdout pipe.
    pub restart_receiver: mpsc::Receiver<oneshot::Sender<()>>,
    /// Notifies [`run_ghci`][crate::ghci::manager::run_ghci] when `ghci` exits unexpectedly so
    /// it can restart the session. Only sent on the unexpected-exit path; intentional restarts
    /// go through [`restart_receiver`][GhciProcess::restart_receiver] instead and do not send
    /// here.
    pub exited_sender: mpsc::Sender<ExitStatus>,
    /// Commands to run before an intentional SIGKILL.
    pub before_kill: Vec<ClonableCommand>,
}

impl GhciProcess {
    #[instrument(skip_all, name = "ghci_process", level = "debug")]
    pub async fn run(mut self, mut process: AsyncGroupChild) -> eyre::Result<()> {
        // We can only call `wait()` once at a time, so we store the future and pass it into the
        // `stop()` handler.
        let mut wait = std::pin::pin!(process.wait());
        tokio::select! {
            _ = self.shutdown.on_shutdown_requested() => {
                self.stop(wait).await?;
            }
            ack = self.restart_receiver.recv() => {
                tracing::debug!("ghci is being shut down");
                self.stop(wait).await?;
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            result = &mut wait => {
                tracing::debug!(?result, "ghci exited");
                let status = result?;
                self.exited(status).await;
                let _ = self.exited_sender.send(status).await;
            }
        }
        Ok(())
    }

    #[instrument(skip_all, level = "debug")]
    async fn stop(
        &self,
        wait: Pin<&mut impl Future<Output = Result<ExitStatus, std::io::Error>>>,
    ) -> eyre::Result<()> {
        run_before_signal_commands(
            &self.before_kill,
            self.process_id,
            self.process_group_id,
            "kill",
        )
        .await;
        kill_process_tree(self.process_id, self.process_group_id)
            .wrap_err("Failed to kill ghci process tree")?;
        // Report the exit status.
        let status = wait.await?;

        self.exited(status).await;
        Ok(())
    }

    async fn exited(&self, status: ExitStatus) {
        tracing::debug!("ghci exited: {status}");
    }
}

/// Run configured diagnostic commands before signaling GHCi.
///
/// A diagnostic failure must never prevent the signal: a wedged command still needs recovery and
/// shutdown must remain reliable.
pub(super) async fn run_before_signal_commands(
    commands: &[ClonableCommand],
    process_id: Pid,
    process_group_id: Pid,
    signal_action: &str,
) {
    for command in commands {
        tracing::info!(%command, "Running before-{signal_action} command");
        let mut child = command.as_tokio();
        child
            .env("GHCIWATCH_PID", process_id.as_raw().to_string())
            .env("GHCIWATCH_PGID", process_group_id.as_raw().to_string())
            .kill_on_drop(true);
        let mut child = match child.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%command, %error, "Failed to run before-{signal_action} command");
                continue;
            }
        };
        match tokio::time::timeout(BEFORE_SIGNAL_COMMAND_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) if status.success() => {
                tracing::debug!(%command, %status, "Before-{signal_action} command completed");
            }
            Ok(Ok(status)) => {
                tracing::warn!(%command, %status, "Before-{signal_action} command failed");
            }
            Ok(Err(error)) => {
                tracing::warn!(%command, %error, "Failed to run before-{signal_action} command");
            }
            Err(_) => {
                tracing::warn!(
                    %command,
                    timeout = ?BEFORE_SIGNAL_COMMAND_TIMEOUT,
                    "Before-{signal_action} command timed out; terminating it"
                );
                // Do not await process exit here: an uninterruptible diagnostic must not delay the
                // GHCi signal past the diagnostic timeout.
                if let Err(error) = child.start_kill() {
                    tracing::warn!(%command, %error, "Failed to terminate timed-out before-{signal_action} command");
                }
            }
        }
    }
}

/// Kill the command process and every descendant we can identify, including descendants that
/// moved themselves into another process group.
///
/// The process group remains an important first line of defense. On Linux we additionally freeze
/// the group, repeatedly discover and freeze descendants through `/proc`, then kill every captured
/// PID. Freezing before killing prevents an ordinary child from forking into the gap between tree
/// discovery and signal delivery.
pub(super) fn kill_process_tree(process_id: Pid, process_group_id: Pid) -> eyre::Result<()> {
    tracing::debug!(
        pid = process_id.as_raw(),
        pgid = process_group_id.as_raw(),
        "Killing ghci command process tree with SIGKILL"
    );

    #[cfg(target_os = "linux")]
    {
        kill_process_tree_linux(process_id, process_group_id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        kill_process_group(process_group_id, Signal::SIGKILL)
    }
}

fn kill_process_group(process_group_id: Pid, signal_to_send: Signal) -> eyre::Result<()> {
    match signal::killpg(process_group_id, signal_to_send) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "Failed to send {signal_to_send:?} to ghci process group {}",
                process_group_id.as_raw()
            )
        }),
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessStat {
    pid: Pid,
    parent_pid: Pid,
    process_group_id: Pid,
    start_time: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct TrackedProcess {
    stat: ProcessStat,
    pid_fd: Option<OwnedFd>,
}

#[cfg(target_os = "linux")]
fn kill_process_tree_linux(process_id: Pid, process_group_id: Pid) -> eyre::Result<()> {
    if let Err(error) = kill_process_group(process_group_id, Signal::SIGSTOP) {
        // Continue: direct descendant SIGSTOPs and the final SIGKILL may still succeed.
        tracing::warn!(%error, "Failed to freeze ghci process group before tree discovery");
    }
    let mut captured = BTreeMap::<i32, TrackedProcess>::new();
    let mut first_kill_error: Option<eyre::Report> = None;

    // Every process found in one pass is stopped before the next pass. Consequently, a stable pass
    // means successfully frozen processes cannot create another descendant before SIGKILL. Keep
    // scanning after a freeze error as well: a vanished process may already have created children
    // which become visible only in the next snapshot.
    loop {
        let snapshot = match process_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::error!(
                    %error,
                    "Failed to enumerate the ghci process tree; falling back to its process group"
                );
                first_kill_error.get_or_insert_with(|| error.into());
                break;
            }
        };
        let processes = process_tree(process_id, process_group_id, &snapshot);
        let mut found_new = false;
        for stat in processes {
            if captured.contains_key(&stat.pid.as_raw()) {
                continue;
            }
            // Retry discovery if this identity disappears between the snapshot and pidfd/stat
            // validation. A later snapshot will either capture it or confirm it is gone.
            found_new = true;
            let Some(process) = track_process(stat) else {
                continue;
            };
            if let Err(error) = signal_tracked_process(&process, Signal::SIGSTOP) {
                // Continue and attempt SIGKILL. Permission errors will be reported again there;
                // transient freeze failures do not mean the subsequent kill failed.
                tracing::warn!(pid = stat.pid.as_raw(), %error, "Failed to freeze ghci descendant");
            }
            captured.insert(stat.pid.as_raw(), process);
        }
        if !found_new {
            break;
        }
    }

    // Kill explicitly captured descendants as well as the group. Pidfds make signal delivery
    // immune to PID reuse on modern Linux; the start-time-checked fallback supports older kernels.
    for process in captured.values().rev() {
        if let Err(error) = signal_tracked_process(process, Signal::SIGKILL) {
            tracing::warn!(
                pid = process.stat.pid.as_raw(),
                %error,
                "Failed to kill ghci descendant"
            );
            first_kill_error.get_or_insert(error);
        }
    }
    if let Err(error) = kill_process_group(process_group_id, Signal::SIGKILL) {
        first_kill_error.get_or_insert(error);
    }

    match first_kill_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn process_snapshot() -> std::io::Result<BTreeMap<i32, ProcessStat>> {
    let mut snapshot = BTreeMap::new();
    for entry in fs::read_dir("/proc")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| parse_process_stat(pid, &stat))
        {
            Some(stat) => stat,
            None => continue,
        };
        snapshot.insert(pid, stat);
    }
    Ok(snapshot)
}

#[cfg(target_os = "linux")]
fn process_tree(
    process_id: Pid,
    process_group_id: Pid,
    snapshot: &BTreeMap<i32, ProcessStat>,
) -> Vec<ProcessStat> {
    // A wrapper command may exit after launching GHCi. Seed discovery with every surviving member
    // of the original group so those processes still anchor descendants which escaped the group.
    let mut family = snapshot
        .values()
        .filter(|stat| stat.pid == process_id || stat.process_group_id == process_group_id)
        .map(|stat| stat.pid.as_raw())
        .collect::<BTreeSet<_>>();
    family.insert(process_id.as_raw());
    let mut processes = family
        .iter()
        .filter_map(|pid| snapshot.get(pid).copied())
        .collect::<Vec<_>>();
    loop {
        let mut changed = false;
        for stat in snapshot.values().copied() {
            if !family.contains(&stat.pid.as_raw()) && family.contains(&stat.parent_pid.as_raw()) {
                family.insert(stat.pid.as_raw());
                processes.push(stat);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    processes
}

#[cfg(target_os = "linux")]
fn track_process(expected: ProcessStat) -> Option<TrackedProcess> {
    // pidfd_open pins the process identity, eliminating the validate-then-signal PID reuse race.
    // Linux before 5.3 does not implement pidfds, so retain the start-time-checked fallback.
    // SAFETY: pidfd_open does not dereference userspace pointers; on success we immediately take
    // ownership of the returned descriptor.
    let raw_fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, expected.pid.as_raw(), 0) };
    let pid_fd = if raw_fd >= 0 {
        // SAFETY: a successful pidfd_open returns a new owned file descriptor.
        let pid_fd = unsafe { OwnedFd::from_raw_fd(raw_fd as i32) };
        if !process_identity_matches(expected) {
            return None;
        }
        Some(pid_fd)
    } else {
        if nix::errno::Errno::last() == nix::errno::Errno::ESRCH {
            return None;
        }
        None
    };
    Some(TrackedProcess {
        stat: expected,
        pid_fd,
    })
}

#[cfg(target_os = "linux")]
fn signal_tracked_process(process: &TrackedProcess, signal_to_send: Signal) -> eyre::Result<()> {
    if let Some(pid_fd) = &process.pid_fd {
        // SAFETY: the pidfd is owned and valid, the signal number comes from nix, and a null
        // siginfo pointer is explicitly supported by pidfd_send_signal(2).
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pidfd_send_signal,
                pid_fd.as_raw_fd(),
                signal_to_send as i32,
                std::ptr::null::<nix::libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = nix::errno::Errno::last();
        return if error == nix::errno::Errno::ESRCH {
            Ok(())
        } else {
            Err(error).wrap_err_with(|| {
                format!(
                    "Failed to send {signal_to_send:?} through pidfd for process {}",
                    process.stat.pid
                )
            })
        };
    }

    if !process_identity_matches(process.stat) {
        tracing::debug!(
            pid = process.stat.pid.as_raw(),
            "Skipped a reused process ID"
        );
        return Ok(());
    }
    match signal::kill(process.stat.pid, signal_to_send) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "Failed to send {signal_to_send:?} to process {}",
                process.stat.pid
            )
        }),
    }
}

#[cfg(target_os = "linux")]
fn process_identity_matches(expected: ProcessStat) -> bool {
    current_process_stat(expected.pid)
        .is_some_and(|current| current.start_time == expected.start_time)
}

#[cfg(target_os = "linux")]
fn current_process_stat(pid: Pid) -> Option<ProcessStat> {
    fs::read_to_string(format!("/proc/{}/stat", pid.as_raw()))
        .ok()
        .and_then(|stat| parse_process_stat(pid.as_raw(), &stat))
}

#[cfg(target_os = "linux")]
fn parse_process_stat(pid: i32, stat: &str) -> Option<ProcessStat> {
    // The comm field is parenthesized and may itself contain spaces or `)`, so split after its
    // final closing parenthesis. Fields below are numbered as documented in proc_pid_stat(5).
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(ProcessStat {
        pid: Pid::from_raw(pid),
        parent_pid: Pid::from_raw(fields.get(1)?.parse().ok()?), // field 4: ppid
        process_group_id: Pid::from_raw(fields.get(2)?.parse().ok()?), // field 5: pgrp
        start_time: fields.get(19)?.parse().ok()?,               // field 22: starttime
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::BTreeMap;

    use nix::unistd::Pid;

    use super::parse_process_stat;
    use super::process_tree;
    use super::ProcessStat;

    #[test]
    fn parses_proc_stat_with_difficult_command_name() {
        let stat = "42 (ghc worker) name) S 7 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 99 20";
        let parsed = parse_process_stat(42, stat).unwrap();
        assert_eq!(parsed.pid.as_raw(), 42);
        assert_eq!(parsed.parent_pid.as_raw(), 7);
        assert_eq!(parsed.process_group_id.as_raw(), 2);
        assert_eq!(parsed.start_time, 99);
    }

    #[test]
    fn finds_descendants_across_multiple_generations() {
        let stat = |pid, parent_pid, process_group_id| ProcessStat {
            pid: Pid::from_raw(pid),
            parent_pid: Pid::from_raw(parent_pid),
            process_group_id: Pid::from_raw(process_group_id),
            start_time: pid as u64,
        };
        let snapshot = BTreeMap::from([
            (10, stat(10, 1, 10)),
            (20, stat(20, 10, 10)),
            (30, stat(30, 20, 30)),
            // A reparented original-group member still anchors its escaped child.
            (50, stat(50, 1, 10)),
            (60, stat(60, 50, 60)),
            (40, stat(40, 1, 40)),
        ]);
        let mut found = process_tree(Pid::from_raw(10), Pid::from_raw(10), &snapshot)
            .into_iter()
            .map(|stat| stat.pid.as_raw())
            .collect::<Vec<_>>();
        found.sort_unstable();
        assert_eq!(found, [10, 20, 30, 50, 60]);
    }
}
