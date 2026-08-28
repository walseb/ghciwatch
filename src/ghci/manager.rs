//! Subsystem for [`Ghci`] to support graceful shutdown.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8PathBuf;
use eyre::Context;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tracing::instrument;

use crate::event_filter::FileEvent;
use crate::event_filter::FileState;
use crate::event_filter::SourceSnapshot;
use crate::ghci::CompilationLog;
use crate::hooks;
use crate::hooks::LifecycleEvent;
use crate::shutdown::ShutdownHandle;
use crate::watcher::WatcherCommand;

use super::memory::format_bytes;
use super::memory::repl_resident_memory;
use super::print_ghciwatch_error;
use super::Ghci;
use super::GhciOpts;
use super::GhciRecoveryFailed;
use super::GhciReloadKind;
/// Delay before retrying a crashed command. This makes one automatic retry visible and prevents
/// an immediate hot loop while still recovering implementation/toolchain failures without an edit.
const CRASH_RESTART_DELAY: Duration = Duration::from_secs(10);
const MEMORY_WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
/// Resident-memory limit for the persistent interactive GHC and its immediate Cabal parent.
const GHCI_MEMORY_LIMIT_BYTES: u64 = 28 * 1024 * 1024 * 1024;

/// An event sent to [`Ghci`] by the watcher.
#[derive(Debug, Clone)]
pub enum WatcherEvent {
    /// Reload the `ghci` session.
    Reload {
        /// The file events to respond to.
        events: BTreeSet<FileEvent>,
        /// State of each event path when this watcher batch was delivered.
        states: BTreeMap<Utf8PathBuf, FileState>,
        /// A stable snapshot of Haskell files under all watch roots.
        haskell_files: BTreeSet<Utf8PathBuf>,
        /// Content snapshot against which this compilation result must be validated.
        source_snapshot: SourceSnapshot,
        /// Whether a temporary startup-retry watch produced this event.
        startup_retry: bool,
    },
}

impl WatcherEvent {
    /// When we interrupt an event to reload, add the file events together so that we don't lose
    /// work.
    fn merge(&mut self, other: WatcherEvent) {
        match (self, other) {
            (
                WatcherEvent::Reload {
                    events,
                    states,
                    haskell_files,
                    source_snapshot,
                    startup_retry,
                },
                WatcherEvent::Reload {
                    events: other_events,
                    states: other_states,
                    haskell_files: other_haskell_files,
                    source_snapshot: other_source_snapshot,
                    startup_retry: other_startup_retry,
                },
            ) => {
                // Keep only the newest event kind and captured state for each path.
                for other_event in other_events {
                    events.retain(|event| event.as_path() != other_event.as_path());
                    events.insert(other_event);
                }
                states.extend(other_states);
                // The later filesystem snapshot supersedes the earlier one.
                *haskell_files = other_haskell_files;
                *source_snapshot = other_source_snapshot;
                *startup_retry |= other_startup_retry;
            }
        }
    }

    /// Remove event hints whose captured file contents have already been dispatched.
    ///
    /// Returns `true` if neither file contents nor the complete Haskell-file snapshot changed.
    fn discard_applied(
        &mut self,
        applied_states: &BTreeMap<Utf8PathBuf, FileState>,
        applied_haskell_files: Option<&BTreeSet<Utf8PathBuf>>,
    ) -> bool {
        match self {
            Self::Reload {
                events,
                states,
                haskell_files,
                ..
            } => {
                events.retain(|event| {
                    states.get(event.as_path()) != applied_states.get(event.as_path())
                });
                events.is_empty()
                    && applied_haskell_files.is_some_and(|applied| applied == haskell_files)
            }
        }
    }

    fn mark_applied(
        &self,
        applied_states: &mut BTreeMap<Utf8PathBuf, FileState>,
        applied_haskell_files: &mut Option<BTreeSet<Utf8PathBuf>>,
    ) {
        match self {
            Self::Reload {
                states,
                haskell_files,
                ..
            } => {
                applied_states.extend(states.clone());
                *applied_haskell_files = Some(haskell_files.clone());
            }
        }
    }

    fn haskell_files(&self) -> BTreeSet<Utf8PathBuf> {
        match self {
            Self::Reload { haskell_files, .. } => haskell_files.clone(),
        }
    }

    /// Paths whose watcher events were being dispatched when GHCi exited.
    fn affected_paths(&self) -> Option<String> {
        let events = match self {
            Self::Reload { events, .. } => events,
        };
        if events.is_empty() {
            return None;
        }
        Some(
            events
                .iter()
                .map(|event| format!("  - {}", event.as_path()))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// Start the [`Ghci`] subsystem.
#[instrument(skip_all, level = "debug")]
pub async fn run_ghci(
    mut handle: ShutdownHandle,
    opts: GhciOpts,
    mut watcher_receiver: mpsc::Receiver<WatcherEvent>,
    watcher_command_sender: mpsc::Sender<WatcherCommand>,
) -> eyre::Result<()> {
    // This function is pretty tricky! We need to handle shutdowns at each stage, and the process
    // is a little different each time, so the `select!`s can't be consolidated.

    let eval_socket = opts.eval_socket.clone().into_std_path_buf();
    let interrupt_reloads = opts.interrupt_reloads;
    let restart_on_exit = opts.restart_on_exit;
    // Keep a manager-side copy so watcher-triggered shell hooks can run before waiting for an
    // active eval or for access to the GHCi session.
    let hooks = opts.hooks.clone();
    let (exited_sender, mut exited_receiver) = mpsc::channel::<ExitStatus>(1);
    let mut ghci = Ghci::new(handle.clone(), opts, exited_sender)
        .await
        .wrap_err("Failed to start `ghci`")?;

    // Wait for ghci to finish loading.
    //
    // NB: We wait for the `ghci.initialize()` call to complete _even if_ `ghci` exits mid-way
    // through; this lets us read the rest of its output and write a compilation log for startup
    // errors.
    let mut log = CompilationLog::default();
    let startup_result = tokio::select! {
        _ = handle.on_shutdown_requested() => {
            ghci.stop().await.wrap_err("Failed to quit ghci")?;
            return Ok(());
        }
        startup_result = ghci.initialize(
            &mut log,
            [LifecycleEvent::Startup(hooks::When::After)],
            None,
        ) => startup_result,
    };
    let startup_exit: Option<ExitStatus> = match startup_result {
        // Even on success, ghci may have exited right after starting up; check for a
        // pending exit status so we don't hand the manager a dead session.
        Ok(()) => exited_receiver.try_recv().ok(),
        Err(err) if is_broken_pipe(&err) || is_recovery_failure(&err) => {
            // GHCi exited during startup, or failed prompt recovery force-killed it.
            // `GhciProcess` delivers the status in both cases. Wait rather than racing
            // `try_recv`, so retry policy attributes the failure to startup correctly.
            tracing::debug!("ghci exited during startup: {err}");
            tokio::select! {
                _ = handle.on_shutdown_requested() => return Ok(()),
                status = exited_receiver.recv() => status,
            }
        }
        Err(err) => return Err(err),
    };
    let mut startup_applied_event = None;
    if let Some(status) = startup_exit {
        match wait_and_restart(
            &mut handle,
            &mut watcher_receiver,
            &mut exited_receiver,
            status,
            "during initial GHCi startup",
            &mut RestartStrategy::Startup(&mut ghci),
            None,
            restart_on_exit,
        )
        .await?
        {
            RetryResult::Restarted(event) => startup_applied_event = event,
            RetryResult::Shutdown => return Ok(()),
        }
    }

    let ghci = Arc::new(Mutex::new(ghci));
    let eval_barrier = crate::my::EvalBarrier::new();
    crate::my::spawn(ghci.clone(), eval_barrier.clone(), eval_socket).await?;
    let mut memory_watchdog = tokio::time::interval(MEMORY_WATCHDOG_INTERVAL);
    memory_watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut manager = GhciManager {
        ghci,
        eval_barrier,
        handle,
        watcher_receiver,
        exited_receiver,
        watcher_command_sender,
        interrupt_reloads,
        restart_on_exit,
        hooks,
        command_handles: Vec::new(),
        memory_watchdog,
        applied_states: BTreeMap::new(),
        applied_haskell_files: None,
    };
    if let Some(event) = startup_applied_event {
        event.mark_applied(
            &mut manager.applied_states,
            &mut manager.applied_haskell_files,
        );
    }
    manager.run().await
}

#[instrument(
    level = "debug",
    skip(ghci, reload_state_sender, watcher_command_sender)
)]
async fn dispatch(
    ghci: Arc<Mutex<Ghci>>,
    event: WatcherEvent,
    reload_state_sender: watch::Sender<GhciReloadKind>,
    watcher_command_sender: mpsc::Sender<WatcherCommand>,
) -> eyre::Result<()> {
    match event {
        WatcherEvent::Reload {
            events,
            haskell_files,
            source_snapshot,
            ..
        } => {
            ghci.lock()
                .await
                .reload(
                    events,
                    haskell_files,
                    source_snapshot,
                    reload_state_sender,
                    watcher_command_sender,
                )
                .await?;
        }
    }
    Ok(())
}

/// Should we interrupt a reload with a new event?
#[instrument(level = "debug", skip_all)]
async fn should_interrupt(mut reload_state_receiver: watch::Receiver<GhciReloadKind>) -> bool {
    loop {
        let reload_kind = *reload_state_receiver.borrow_and_update();
        match reload_kind {
            GhciReloadKind::Pending => {
                if let Err(err) = reload_state_receiver.changed().await {
                    tracing::debug!("Failed to receive reload state from ghci: {err}");
                    return false;
                }
            }
            GhciReloadKind::None | GhciReloadKind::Restart => {
                // Nothing to do, wait for the task to finish. `None` also marks the
                // post-compilation hooks, which must not be interrupted by a later edit.
                tracing::debug!(?reload_kind, "Not interrupting reload");
                return false;
            }
            GhciReloadKind::Reload => {
                tracing::debug!(?reload_kind, "Interrupting reload");
                return true;
            }
        }
    }
}

/// Manages the main event loop for a running ghci session.
struct GhciManager {
    ghci: Arc<Mutex<Ghci>>,
    eval_barrier: Arc<crate::my::EvalBarrier>,
    handle: ShutdownHandle,
    watcher_receiver: mpsc::Receiver<WatcherEvent>,
    exited_receiver: mpsc::Receiver<ExitStatus>,
    /// Requests publication-time source rescans from the watcher.
    watcher_command_sender: mpsc::Sender<WatcherCommand>,
    interrupt_reloads: bool,
    restart_on_exit: bool,
    /// Hook configuration copied out of GHCi so before-reload shell hooks do not wait on it.
    hooks: crate::hooks::HookOpts,
    /// Background `async:` before-reload shell hooks owned by the manager.
    command_handles: Vec<tokio::task::JoinHandle<eyre::Result<ExitStatus>>>,
    memory_watchdog: tokio::time::Interval,
    /// File states represented by the last successfully completed dispatch.
    applied_states: BTreeMap<Utf8PathBuf, FileState>,
    /// Complete watched-source snapshot represented by the last completed dispatch.
    applied_haskell_files: Option<BTreeSet<Utf8PathBuf>>,
}

/// Result of [`GhciManager::wait_for_event`].
enum WaitResult {
    /// A watcher event was received.
    Event(WatcherEvent),
    /// A shutdown was requested (or the watcher channel closed).
    Shutdown,
    /// ghci died and was successfully restarted; caller should continue the loop.
    Restarted,
}

/// Result of [`GhciManager::handle_event`].
enum HandleResult {
    /// The event was dispatched (or ghci died during dispatch but was restarted).
    Done,
    /// The reload was interrupted; the merged event should be retried next iteration.
    Interrupted(WatcherEvent),
    /// A shutdown was requested.
    Shutdown,
}

impl GhciManager {
    async fn run(mut self) -> eyre::Result<()> {
        let mut maybe_event: Option<WatcherEvent> = None;
        loop {
            let event = match maybe_event.take() {
                Some(event) => event,
                None => match self.wait_for_event().await? {
                    WaitResult::Event(event) => event,
                    WaitResult::Shutdown => break,
                    WaitResult::Restarted => continue,
                },
            };
            match self.handle_event(event).await? {
                HandleResult::Done => {}
                HandleResult::Interrupted(event) => maybe_event = Some(event),
                HandleResult::Shutdown => break,
            }
        }

        Ok(())
    }

    /// Wait for the next watcher event, handling shutdown, ghci death, and memory checks.
    async fn wait_for_event(&mut self) -> eyre::Result<WaitResult> {
        enum Wake {
            GhciExited(ExitStatus),
            MemoryWatchdog,
        }

        loop {
            let wake = {
                let GhciManager {
                    ref ghci,
                    ref mut handle,
                    ref mut watcher_receiver,
                    ref mut exited_receiver,
                    ref mut memory_watchdog,
                    ref applied_states,
                    ref applied_haskell_files,
                    ..
                } = *self;
                tokio::select! {
                    _ = handle.on_shutdown_requested() => {
                        ghci.lock().await.stop().await
                            .wrap_err("Failed to quit ghci")?;
                        return Ok(WaitResult::Shutdown);
                    }
                    ret = watcher_receiver.recv() => {
                        match ret {
                            Some(mut event) => {
                                tracing::debug!(?event, "Received ghci event from watcher");
                                if event.discard_applied(
                                    applied_states,
                                    applied_haskell_files.as_ref(),
                                ) {
                                    tracing::debug!("Discarding watcher event already applied to GHCi");
                                    continue;
                                }
                                return Ok(WaitResult::Event(event));
                            }
                            None => {
                                // Channel closed — shutdown in progress.
                                tracing::debug!(
                                    "Watcher event channel closed; shutting down"
                                );
                                ghci.lock().await.stop().await
                                    .wrap_err("Failed to quit ghci")?;
                                return Ok(WaitResult::Shutdown);
                            }
                        }
                    }
                    Some(status) = exited_receiver.recv() => Wake::GhciExited(status),
                    _ = memory_watchdog.tick() => Wake::MemoryWatchdog,
                }
            };
            // self is no longer partially borrowed, so lock ordering remains the same
            // as eval/reload: operation barrier first, GHCi mutex second.
            match wake {
                Wake::MemoryWatchdog => {
                    if self.run_memory_watchdog().await? {
                        return Ok(WaitResult::Restarted);
                    }
                }
                Wake::GhciExited(status) => {
                    // Failed SIGINT recovery immediately replaces a session that was killed while
                    // restoring protocol synchronization. Other exits use the common delayed retry
                    // loop, including persistent services, so watcher snapshots are not lost.
                    let recovery_kill = {
                        let _operation = self.eval_barrier.begin_operation().await;
                        let mut ghci = self.ghci.lock().await;
                        if ghci.recovery_restart_required() {
                            ghci.restart_after_recovery_kill_with_known_files()
                                .await
                                .wrap_err("Failed to restart GHCi after unexpected exit")?;
                            true
                        } else {
                            false
                        }
                    };
                    if recovery_kill {
                        return Ok(WaitResult::Restarted);
                    }
                    return match self
                        .wait_and_restart_runtime(
                            status,
                            "while waiting for filesystem events",
                            None,
                        )
                        .await?
                    {
                        RetryResult::Restarted(Some(event)) => {
                            event.mark_applied(
                                &mut self.applied_states,
                                &mut self.applied_haskell_files,
                            );
                            Ok(WaitResult::Restarted)
                        }
                        RetryResult::Restarted(None) => Ok(WaitResult::Restarted),
                        RetryResult::Shutdown => Ok(WaitResult::Shutdown),
                    };
                }
            }
        }
    }

    /// Dispatch a watcher event, handling shutdown, interruption, and ghci death.
    ///
    /// Stays running until the dispatch task completes (or we decide to interrupt it),
    /// so the spawned task never outlives this call — otherwise it could keep holding
    /// the ghci `Mutex` and deadlock the next iteration. Events that arrive during a
    /// non-interruptible dispatch are accumulated into `pending_event` and returned as
    /// `Interrupted` for retry.
    async fn handle_event(&mut self, mut event: WatcherEvent) -> eyre::Result<HandleResult> {
        // This event has passed debounce and the applied-state duplicate check. Notify external
        // consumers now, before a slow eval or occupied GHCi session can delay the reload cycle.
        self.command_handles.retain(|handle| !handle.is_finished());
        self.hooks
            .run_shell_hooks(
                LifecycleEvent::Reload(hooks::When::Before),
                &mut self.command_handles,
            )
            .await?;

        // Queue behind any active eval and prevent later evals from entering GHCi
        // until this complete dispatch (including interruption cleanup) is done.
        let _eval_reload_guard = self.eval_barrier.begin_operation().await;
        let (reload_state_sender, reload_state_receiver) = watch::channel(GhciReloadKind::Pending);
        let mut task = Box::pin(tokio::task::spawn(dispatch(
            self.ghci.clone(),
            event.clone(),
            reload_state_sender,
            self.watcher_command_sender.clone(),
        )));

        // We only need one interrupt decision. The watch receiver also lets GHCi mark the
        // post-compilation hook phase as non-interruptible without racing a watcher event.
        let mut reload_state_receiver = Some(reload_state_receiver);
        // Events that arrive while we're waiting for a non-interruptible dispatch
        // (e.g. a restart) to complete. Returned as `Interrupted` for retry.
        let mut pending_event: Option<WatcherEvent> = None;
        // A due check waits for this serialized reload/restart to finish; it never
        // aborts a pipe protocol or takes the GHCi mutex out of lock order.
        let mut memory_watchdog_due = false;
        // Failed prompt recovery force-kills GHCi and must trigger an immediate
        // replacement, rather than the unexpected-exit policy of waiting for a file change.
        let mut restart_after_recovery_kill = false;

        let ghci_exited = loop {
            let GhciManager {
                ref ghci,
                ref mut handle,
                ref mut watcher_receiver,
                ref mut exited_receiver,
                ref mut memory_watchdog,
                interrupt_reloads,
                ref mut applied_states,
                ref mut applied_haskell_files,
                ..
            } = *self;
            break tokio::select! {
                biased;
                _ = handle.on_shutdown_requested() => {
                    // Cancel any in-progress reloads. This releases the lock so we don't
                    // block here.
                    task.abort();
                    ghci.lock().await.stop().await
                        .wrap_err("Failed to quit ghci")?;
                    return Ok(HandleResult::Shutdown);
                }
                Some(status) = exited_receiver.recv() => {
                    // The command can exit while a restart is still collecting pre-GHCi build
                    // diagnostics. Let dispatch finish draining the closed pipes and writing the
                    // error file before releasing its mutex. This mirrors initial startup, where
                    // process exit likewise does not cancel initialization.
                    let dispatch_result = tokio::select! {
                        _ = handle.on_shutdown_requested() => {
                            task.abort();
                            return Ok(HandleResult::Shutdown);
                        }
                        ret = &mut task => ret?,
                    };
                    if let Err(err) = dispatch_result {
                        if is_recovery_failure(&err) {
                            restart_after_recovery_kill = true;
                            preserve_recovery_event(&event, &mut pending_event);
                        } else if !is_broken_pipe(&err) {
                            return Err(err);
                        }
                        tracing::debug!("ghci exited while dispatching: {err}");
                    }
                    if ghci.lock().await.recovery_restart_required() {
                        restart_after_recovery_kill = true;
                        preserve_recovery_event(&event, &mut pending_event);
                    }
                    Some(status)
                }
                _ = memory_watchdog.tick() => {
                    memory_watchdog_due = true;
                    continue;
                }
                Some(mut new_event) = watcher_receiver.recv() => {
                    // Drain any other events already queued up so we treat a burst
                    // as one decision point — otherwise we'd loop once per event,
                    // and on interrupt we'd only fold in the first one and trigger
                    // another interrupt on the next iteration.
                    drain_pending(&mut new_event, watcher_receiver);
                    tracing::debug!(
                        ?new_event,
                        "Received ghci event from watcher while reloading"
                    );

                    // Retain every event until dispatch completes. We can then discard captured
                    // states that the completed reload already applied; a different state or full
                    // source snapshot remains dirty for one follow-up cycle.

                    // Check if we should interrupt the in-progress reload. We can only
                    // check once (the state receiver is consumed), and only for interruptible
                    // reloads.
                    if interrupt_reloads {
                        if let Some(reload_state_receiver) = reload_state_receiver.take() {
                            if should_interrupt(reload_state_receiver).await {
                                // Merge everything: any previously accumulated events
                                // plus the newest event.
                                if let Some(pending_event) = pending_event.take() {
                                    event.merge(pending_event);
                                }
                                event.merge(new_event);

                                // Cancel the in-progress reload. This releases the
                                // `ghci` lock to prevent a deadlock.
                                task.abort();

                                // Send a SIGINT to interrupt the reload.
                                // NB: This may take a couple seconds to register.
                                let mut ghci = ghci.lock().await;
                                match ghci.send_sigint().await {
                                    Ok(()) => {
                                        ghci.finish_interrupted_reload(true).await?;
                                        return Ok(HandleResult::Interrupted(event));
                                    }
                                    Err(e) => {
                                        // `send_sigint` force-kills the session if prompt
                                        // synchronization cannot be restored. Shell after-hooks can
                                        // still balance the completed attempt.
                                        ghci.finish_interrupted_reload(false).await?;
                                        // Consume the exit status, then immediately initialize a
                                        // replacement with the merged event's filesystem snapshot.
                                        tracing::warn!(
                                            error = ?e,
                                            "Failed to interrupt ghci; session was killed for restart",
                                        );
                                        pending_event = Some(event.clone());
                                        restart_after_recovery_kill = true;
                                        let status = exited_receiver
                                            .recv()
                                            .await
                                            .ok_or_else(|| {
                                                eyre::eyre!(
                                                    "ghci exit channel closed after kill"
                                                )
                                            })?;
                                        break Some(status);
                                    }
                                }
                            }
                        }
                    }

                    // Either `interrupt_reloads` is `false`, the state receiver was already
                    // consumed, or the operation is non-interruptible. Accumulate the event
                    // and keep waiting for the dispatch task to finish.
                    match pending_event {
                        Some(ref mut pending_event) => pending_event.merge(new_event),
                        None => pending_event = Some(new_event),
                    }

                    // Loop around to make sure we keep waiting for the `task`.
                    continue;
                }
                ret = &mut task => {
                    match ret? {
                        Ok(()) => {
                            tracing::debug!("Finished dispatching ghci event");
                            event.mark_applied(applied_states, applied_haskell_files);
                            None
                        }
                        Err(err) if is_broken_pipe(&err) || is_recovery_failure(&err) => {
                            // GHCi died during dispatch. A broken pipe follows the normal
                            // unexpected-exit policy; a typed recovery failure means
                            // send_sigint force-killed an unsynchronized session and requires
                            // an immediate replacement. In either case, consume the matching
                            // process-exit notification before continuing.
                            if is_recovery_failure(&err)
                                || ghci.lock().await.recovery_restart_required()
                            {
                                restart_after_recovery_kill = true;
                                preserve_recovery_event(&event, &mut pending_event);
                            }
                            tracing::debug!("ghci exited while dispatching: {err}");
                            tokio::select! {
                                _ = handle.on_shutdown_requested() => {
                                    // ghci is already dead; nothing to stop.
                                    return Ok(HandleResult::Shutdown);
                                }
                                status = exited_receiver.recv() => match status {
                                    Some(status) => Some(status),
                                    // Channel closed -- shutdown in progress.
                                    None => return Ok(HandleResult::Shutdown),
                                },
                            }
                        }
                        Err(err) => return Err(err),
                    }
                }
            };
        };

        if let Some(status) = ghci_exited {
            if restart_after_recovery_kill {
                tracing::warn!(%status, "Restarting GHCi immediately after failed interrupt recovery");
                let haskell_files = pending_event
                    .take()
                    .expect("recovery kill must preserve its triggering event")
                    .haskell_files();
                self.ghci
                    .lock()
                    .await
                    .restart_after_recovery_kill(haskell_files)
                    .await
                    .wrap_err("Failed to restart GHCi after unsuccessful interrupt recovery")?;
            } else {
                // Retry the exact filesystem state once. Only a second crash with no newer
                // watcher event is treated as a crash loop, unless persistent recovery is enabled.
                match self
                    .wait_and_restart_runtime(
                        status,
                        "while dispatching a reload/restart event",
                        Some(event.clone()),
                    )
                    .await?
                {
                    RetryResult::Restarted(Some(restart_event)) => {
                        restart_event.mark_applied(
                            &mut self.applied_states,
                            &mut self.applied_haskell_files,
                        );
                    }
                    RetryResult::Restarted(None) => {}
                    RetryResult::Shutdown => return Ok(HandleResult::Shutdown),
                }
            }
        }

        if ghci_exited.is_none() && memory_watchdog_due {
            // The operation permit is still held here, and the dispatch task has released
            // the GHCi mutex. Check and restart without reacquiring the non-reentrant barrier.
            self.run_memory_watchdog_with_operation_permit().await?;
        }

        // Merge events that arrived while dispatch was running. Retry only if their latest
        // captured state or authoritative filesystem snapshot differs from what just completed.
        if let Some(mut pending_event) = pending_event {
            drain_pending(&mut pending_event, &mut self.watcher_receiver);
            if !pending_event
                .discard_applied(&self.applied_states, self.applied_haskell_files.as_ref())
            {
                return Ok(HandleResult::Interrupted(pending_event));
            }
            tracing::debug!("Discarding watcher changes already applied by the completed reload");
        }

        Ok(HandleResult::Done)
    }

    async fn run_memory_watchdog(&self) -> eyre::Result<bool> {
        let _operation_guard = self.eval_barrier.begin_operation().await;
        self.run_memory_watchdog_with_operation_permit().await
    }

    async fn run_memory_watchdog_with_operation_permit(&self) -> eyre::Result<bool> {
        let mut ghci = self.ghci.lock().await;
        let process_id = ghci.process_id();
        let process_group_id = ghci.process_group_id();
        let usage = tokio::task::spawn_blocking(move || {
            repl_resident_memory(process_id.as_raw(), process_group_id.as_raw())
        })
        .await
        .wrap_err("GHCi memory watchdog task failed")?;
        let usage = match usage {
            Ok(usage) => usage,
            Err(error) => {
                tracing::warn!(%error, "Failed to read GHCi repl memory usage");
                return Ok(false);
            }
        };
        tracing::debug!(
            bytes = usage.bytes,
            limit = GHCI_MEMORY_LIMIT_BYTES,
            command_pid = usage.command_pid,
            cabal_parent_pid = usage.cabal_parent.map(|(pid, _)| pid),
            cabal_parent_bytes = usage.cabal_parent.map(|(_, bytes)| bytes),
            interactive_ghc_pid = usage.interactive_ghc.map(|(pid, _)| pid),
            interactive_ghc_bytes = usage.interactive_ghc.map(|(_, bytes)| bytes),
            "Checked GHCi repl resident memory"
        );
        if usage.bytes <= GHCI_MEMORY_LIMIT_BYTES {
            return Ok(false);
        }

        print_ghciwatch_error(
            "GHCi exceeded its resident-memory limit",
            &format!(
                "Component: GHCi and immediate Cabal parent\n{}\nCombined resident memory: {}\nLimit: {}\nRecovery: restarting GHCi through the normal lifecycle-hook and target-synchronization machinery",
                usage.details(),
                format_bytes(usage.bytes),
                format_bytes(GHCI_MEMORY_LIMIT_BYTES),
            ),
        );
        tracing::warn!(
            bytes = usage.bytes,
            limit = GHCI_MEMORY_LIMIT_BYTES,
            "GHCi memory watchdog is restarting the session"
        );
        ghci.restart_for_memory_watchdog()
            .await
            .wrap_err("Failed to restart GHCi after exceeding its memory limit")?;
        Ok(true)
    }

    /// Retry a crashed session once, then wait for a watched change if it crashes again.
    #[instrument(level = "debug", skip_all)]
    async fn wait_and_restart_runtime(
        &mut self,
        status: ExitStatus,
        detected_phase: &'static str,
        initial_event: Option<WatcherEvent>,
    ) -> eyre::Result<RetryResult> {
        wait_and_restart(
            &mut self.handle,
            &mut self.watcher_receiver,
            &mut self.exited_receiver,
            status,
            detected_phase,
            &mut RestartStrategy::Runtime(self.ghci.clone()),
            initial_event,
            self.restart_on_exit,
        )
        .await
    }
}

/// Drain all pending events from the receiver and merge them into `event`.
fn drain_pending(event: &mut WatcherEvent, watcher_receiver: &mut mpsc::Receiver<WatcherEvent>) {
    while let Ok(new_event) = watcher_receiver.try_recv() {
        event.merge(new_event);
    }
}

fn preserve_recovery_event(event: &WatcherEvent, pending_event: &mut Option<WatcherEvent>) {
    let mut recovery_event = event.clone();
    if let Some(newer_event) = pending_event.take() {
        recovery_event.merge(newer_event);
    }
    *pending_event = Some(recovery_event);
}

/// Outcome of [`wait_and_restart`].
enum RetryResult {
    /// GHCi was successfully restarted, optionally using a triggering watcher event.
    Restarted(Option<WatcherEvent>),
    /// A shutdown was requested while waiting.
    Shutdown,
}

/// How to restart ghci — differs between initial startup and runtime.
enum RestartStrategy<'a> {
    /// ghci failed during first startup; use [`Ghci::startup_restart`].
    Startup(&'a mut Ghci),
    /// ghci died at runtime; lock the [`Arc`] and call [`Ghci::startup_restart`].
    Runtime(Arc<Mutex<Ghci>>),
}

impl RestartStrategy<'_> {
    fn context(&self) -> &'static str {
        match self {
            Self::Startup(_) => "during startup",
            Self::Runtime(_) => "unexpectedly",
        }
    }

    async fn restart(
        &mut self,
        haskell_files: Option<BTreeSet<Utf8PathBuf>>,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        match self {
            Self::Startup(ghci) => {
                let haskell_files =
                    haskell_files.unwrap_or_else(|| ghci.known_haskell_files_absolute());
                ghci.startup_restart(haskell_files, log)
                    .await
                    .wrap_err("Failed to restart ghci after startup failure")
            }
            Self::Runtime(ghci) => {
                let mut ghci = ghci.lock().await;
                let haskell_files =
                    haskell_files.unwrap_or_else(|| ghci.known_haskell_files_absolute());
                ghci.startup_restart(haskell_files, log)
                    .await
                    .wrap_err("Failed to restart ghci after unexpected exit")
            }
        }
    }

    async fn diagnostic_context(&mut self) -> String {
        match self {
            Self::Startup(ghci) => ghci.diagnostic_context(),
            Self::Runtime(ghci) => ghci.lock().await.diagnostic_context(),
        }
    }
}

/// Outcome of a replacement initialization attempt in [`wait_and_restart`].
enum RestartRace {
    /// The restart completed successfully.
    Restarted,
    /// Initialization observed the process exit (usually as a broken pipe); its status is pending.
    ExitPending,
}

/// Retry a crashed command after a fixed delay. If that replacement crashes with no watched
/// changes, treat it as a crash loop and wait until any watched path changes before trying again,
/// unless persistent recovery was requested. Changes received during the delay or replacement are
/// merged so initialization always gets the newest complete source snapshot.
async fn wait_and_restart(
    handle: &mut ShutdownHandle,
    watcher_receiver: &mut mpsc::Receiver<WatcherEvent>,
    exited_receiver: &mut mpsc::Receiver<ExitStatus>,
    mut status: ExitStatus,
    detected_phase: &'static str,
    strategy: &mut RestartStrategy<'_>,
    initial_event: Option<WatcherEvent>,
    restart_unchanged: bool,
) -> eyre::Result<RetryResult> {
    let context = strategy.context();
    let initial_affected_paths = initial_event
        .as_ref()
        .and_then(WatcherEvent::affected_paths)
        .map(|paths| format!("\nAffected paths:\n{paths}"))
        .unwrap_or_default();
    let recovery = if restart_unchanged {
        format!(
            "persistently retrying with a fresh GHCi session after {CRASH_RESTART_DELAY:?}, using the newest queued watched state"
        )
    } else {
        format!(
            "retrying with a fresh GHCi session after {CRASH_RESTART_DELAY:?}; if the unchanged replacement also crashes, waiting for any configured watched path to change"
        )
    };
    let details = format!(
        "Detected phase: {detected_phase}\n{}\n{}{initial_affected_paths}\nRecovery: {recovery}",
        exit_status_diagnostic(status),
        strategy.diagnostic_context().await,
    );
    print_ghciwatch_error("GHCi exited unexpectedly", &details);
    tracing::warn!(
        %status,
        restart_unchanged,
        "ghci exited {context}; retrying after crash delay"
    );

    let mut pending_event = initial_event;
    let mut attempted_states = BTreeMap::new();
    let mut attempted_haskell_files = None;
    let mut first_retry = true;

    loop {
        // Every observed crash gets a quiet period. Watcher events continue to accumulate during it.
        let delay = tokio::time::sleep(CRASH_RESTART_DELAY);
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = handle.on_shutdown_requested() => return Ok(RetryResult::Shutdown),
                _ = &mut delay => break,
                event = watcher_receiver.recv() => {
                    let Some(event) = event else {
                        tracing::debug!("Watcher event channel closed; shutting down");
                        return Ok(RetryResult::Shutdown);
                    };
                    match &mut pending_event {
                        Some(pending) => pending.merge(event),
                        None => pending_event = Some(event),
                    }
                }
            }
        }

        if !first_retry && !restart_unchanged {
            // A replacement has already crashed. Delayed duplicate notifications do not break the
            // crash loop; any genuinely newer watched state does, regardless of file classification.
            loop {
                if let Some(mut event) = pending_event.take() {
                    drain_pending(&mut event, watcher_receiver);
                    if !event.discard_applied(&attempted_states, attempted_haskell_files.as_ref()) {
                        pending_event = Some(event);
                        break;
                    }
                    tracing::debug!("Discarding watched change already used by failed replacement");
                }
                let event = tokio::select! {
                    _ = handle.on_shutdown_requested() => return Ok(RetryResult::Shutdown),
                    event = watcher_receiver.recv() => {
                        let Some(event) = event else {
                            tracing::debug!("Watcher event channel closed; shutting down");
                            return Ok(RetryResult::Shutdown);
                        };
                        event
                    }
                };
                pending_event = Some(event);
            }
        }
        first_retry = false;

        let haskell_files = pending_event.as_ref().map(WatcherEvent::haskell_files);
        tracing::debug!("Restarting ghci");
        let mut restart_log = CompilationLog::default();
        // Initialization must drain startup diagnostics and run shell-only after-hooks before the
        // already-buffered process-exit status is consumed below.
        let race = match strategy.restart(haskell_files, &mut restart_log).await {
            Ok(()) => RestartRace::Restarted,
            Err(err) if is_broken_pipe(&err) || is_recovery_failure(&err) => {
                tracing::debug!("ghci exited while restarting: {err}");
                RestartRace::ExitPending
            }
            Err(err) => return Err(err),
        };
        let new_status = match race {
            RestartRace::Restarted => match exited_receiver.try_recv() {
                Ok(new_status) => new_status,
                Err(_) => return Ok(RetryResult::Restarted(pending_event)),
            },
            RestartRace::ExitPending => {
                tokio::select! {
                    _ = handle.on_shutdown_requested() => return Ok(RetryResult::Shutdown),
                    new_status = exited_receiver.recv() => match new_status {
                        Some(new_status) => new_status,
                        None => return Ok(RetryResult::Shutdown),
                    },
                }
            }
        };
        status = new_status;

        let affected_paths = pending_event
            .as_ref()
            .and_then(WatcherEvent::affected_paths)
            .map(|paths| format!("\nAffected paths:\n{paths}"))
            .unwrap_or_default();
        if let Some(event) = pending_event.take() {
            // This exact watched state already produced a complete replacement attempt.
            event.mark_applied(&mut attempted_states, &mut attempted_haskell_files);
        }

        // Events can arrive while replacement initialization owns the GHCi protocol. Pull all of
        // them in before describing whether we are waiting or already have newer work to retry.
        while let Ok(event) = watcher_receiver.try_recv() {
            match &mut pending_event {
                Some(pending) => pending.merge(event),
                None => pending_event = Some(event),
            }
        }
        if pending_event.as_mut().is_some_and(|event| {
            event.discard_applied(&attempted_states, attempted_haskell_files.as_ref())
        }) {
            pending_event = None;
        }

        let (recovery, warning) = if restart_unchanged {
            (
                format!(
                    "persistent recovery enabled; retrying after {CRASH_RESTART_DELAY:?} with the newest queued watched state"
                ),
                "persistent recovery will retry unchanged state",
            )
        } else if pending_event.is_some() {
            (
                format!(
                    "a newer watched state is already queued; retrying after {CRASH_RESTART_DELAY:?}"
                ),
                "newer watched state queued for retry",
            )
        } else {
            (
                format!(
                    "crash loop detected; waiting for any configured watched path to change, then retrying after the {CRASH_RESTART_DELAY:?} crash delay"
                ),
                "no newer watched state; waiting for a configured file change",
            )
        };
        let details = format!(
            "Detected phase: while starting a replacement GHCi session\n{}\n{}{affected_paths}\nRecovery: {recovery}",
            exit_status_diagnostic(status),
            strategy.diagnostic_context().await,
        );
        print_ghciwatch_error("GHCi exited again while restarting", &details);
        tracing::warn!(%status, "ghci exited {context}; {warning}");
    }
}

fn exit_status_diagnostic(status: ExitStatus) -> String {
    let signal = status.signal();
    let signal_description = match signal {
        Some(number) => match nix::sys::signal::Signal::try_from(number) {
            Ok(signal) => format!("{number} ({signal:?})"),
            Err(_) => number.to_string(),
        },
        None => "none".to_owned(),
    };
    let hint = match signal {
        Some(number) if number == nix::sys::signal::Signal::SIGKILL as i32 => {
            "\nDiagnostic hint: SIGKILL can indicate the Linux OOM killer or an external kill; check `journalctl -k`/`dmesg` around this timestamp"
        }
        Some(number)
            if [
                nix::sys::signal::Signal::SIGABRT,
                nix::sys::signal::Signal::SIGBUS,
                nix::sys::signal::Signal::SIGILL,
                nix::sys::signal::Signal::SIGSEGV,
            ]
            .iter()
            .any(|signal| *signal as i32 == number) =>
        {
            "\nDiagnostic hint: this signal usually indicates a native-code crash; inspect the core dump if one was produced"
        }
        _ => "",
    };
    format!(
        "Command exit status: {status}\nExit code: {:?}\nTerminating signal: {signal_description}\nCore dumped: {}{hint}\nStatus note: this status belongs to the configured --command process; a wrapper such as cabal may translate a child GHC crash signal into its own exit code",
        status.code(),
        status.core_dumped(),
    )
}

fn is_recovery_failure(err: &eyre::Report) -> bool {
    err.downcast_ref::<GhciRecoveryFailed>().is_some()
        || err
            .chain()
            .any(|error| error.downcast_ref::<GhciRecoveryFailed>().is_some())
}

/// Check whether the error (or anything in its chain) is a broken pipe.
///
/// Reading from or writing to ghci fails with a broken pipe when the process has exited;
/// `GhciProcess` will deliver the exit status on the exit channel shortly afterwards.
fn is_broken_pipe(err: &eyre::Report) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_filter::file_states;
    use std::time::SystemTime;

    fn event(path: &camino::Utf8Path) -> WatcherEvent {
        let events = BTreeSet::from([FileEvent::Modify(path.to_owned())]);
        let states = file_states(&events).unwrap();
        WatcherEvent::Reload {
            events,
            states,
            haskell_files: BTreeSet::from([path.to_owned()]),
            source_snapshot: BTreeMap::from([(path.to_owned(), FileState::capture(path).unwrap())]),
            startup_retry: false,
        }
    }

    #[test]
    fn delayed_duplicate_edit_is_discarded_after_reload() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ghciwatch-manager-{}-{unique}.hs",
            std::process::id()
        ));
        let path = camino::Utf8PathBuf::try_from(path).unwrap();
        std::fs::write(&path, "module First where\nvalue = 1\n").unwrap();

        let first = event(&path);
        let mut applied_states = BTreeMap::new();
        let mut applied_haskell_files = None;
        first.mark_applied(&mut applied_states, &mut applied_haskell_files);

        // Keep the replacement the same length to ensure the content hash, rather than only file
        // size (or a coarse timestamp), distinguishes the edit.
        std::fs::write(&path, "module First where\nvalue = 2\n").unwrap();
        let latest = event(&path);
        let mut delayed_duplicate = latest.clone();
        assert!(
            !delayed_duplicate.discard_applied(&applied_states, applied_haskell_files.as_ref(),)
        );

        latest.mark_applied(&mut applied_states, &mut applied_haskell_files);
        assert!(delayed_duplicate.discard_applied(&applied_states, applied_haskell_files.as_ref(),));

        std::fs::remove_file(path).unwrap();
    }
}
