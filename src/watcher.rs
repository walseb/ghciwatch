use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::time::Duration;

use camino::Utf8PathBuf;
use eyre::eyre;
use notify_debouncer_full::notify;
use notify_debouncer_full::notify::PollWatcher;
use notify_debouncer_full::notify::RecommendedWatcher;
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::DebounceEventHandler;
use notify_debouncer_full::DebounceEventResult;
use notify_debouncer_full::Debouncer;
use notify_debouncer_full::FileIdMap;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::block_in_place;
use tracing::instrument;

use crate::cli::Opts;
use crate::event_filter::file_events_from_action;
use crate::event_filter::file_states;
use crate::event_filter::FileEvent;
use crate::event_filter::FileState;
use crate::event_filter::SourceSnapshot;
use crate::ghci::manager::WatcherEvent;
use crate::ghci::FileClassifier;
use crate::haskell_source_file::is_haskell_source_file;
use crate::normal_path::NormalPath;
use crate::shutdown::ShutdownHandle;

/// A command sent by the GHCi manager to the filesystem watcher.
#[derive(Debug)]
pub enum WatcherCommand {
    /// Replace the temporary set of source files that can trigger a retry after GHCi startup fails.
    ///
    /// The reply is sent only after the temporary watcher has been installed. Errors are strings so
    /// the watcher can both report the failure and remain alive for ordinary configured watches.
    SetStartupRetryFiles {
        /// Absolute, normalized files to watch.
        files: BTreeSet<Utf8PathBuf>,
        /// Reports whether the temporary watcher was installed.
        ack: oneshot::Sender<Result<(), String>>,
    },
    /// Remove all temporary startup-retry watches.
    ClearStartupRetryFiles {
        /// Acknowledges that the temporary watcher has stopped.
        ack: oneshot::Sender<()>,
    },
    /// Check that a completed compilation still belongs to the current watched-source snapshot.
    /// If it does not, enqueue a synthetic event carrying the fresh snapshot before replying.
    ValidateSourceSnapshot {
        /// Snapshot captured for the compilation attempt.
        expected: SourceSnapshot,
        /// Reports whether the snapshot is still current after queuing any required follow-up.
        ack: oneshot::Sender<Result<bool, String>>,
    },
}

/// Options for [`run_watcher`]. This is like a lower-effort builder interface, mostly
/// provided because Rust tragically lacks named arguments.
pub struct WatcherOpts {
    /// The paths to watch for changes.
    pub watch: Vec<NormalPath>,
    /// Debounce duration for filesystem events.
    pub debounce: Duration,
    /// If given, use the polling file watcher with the given duration as the poll interval.
    pub poll: Option<Duration>,
    /// Classifies paths before they are sent to the GHCi manager.
    pub file_classifier: FileClassifier,
}

impl WatcherOpts {
    /// Construct options for [`run_watcher`] from parsed command-line interface arguments as [`Opts`].
    ///
    /// This extracts the bits of an [`Opts`] struct relevant to the [`run_watcher`] session
    /// without cloning or taking ownership of the entire thing.
    pub fn from_cli(opts: &Opts) -> eyre::Result<Self> {
        Ok(Self {
            watch: opts.watch.paths.clone(),
            debounce: opts.watch.debounce,
            poll: opts.watch.poll,
            file_classifier: FileClassifier::new(
                opts.watch.restart_globs()?,
                opts.watch.reload_globs()?,
            )?,
        })
    }
}

/// A [`notify`] watcher which waits for file changes and sends reload events to the contained
/// `ghci` session.
#[instrument(level = "debug", skip_all)]
pub async fn run_watcher(
    handle: ShutdownHandle,
    ghci_sender: mpsc::Sender<WatcherEvent>,
    command_receiver: mpsc::Receiver<WatcherCommand>,
    opts: WatcherOpts,
) -> eyre::Result<()> {
    if opts.poll.is_some() {
        run_debouncer::<PollWatcher>(handle, ghci_sender, command_receiver, opts).await
    } else {
        run_debouncer::<RecommendedWatcher>(handle, ghci_sender, command_receiver, opts).await
    }
}

async fn run_debouncer<T: notify::Watcher>(
    mut handle: ShutdownHandle,
    ghci_sender: mpsc::Sender<WatcherEvent>,
    mut command_receiver: mpsc::Receiver<WatcherCommand>,
    opts: WatcherOpts,
) -> eyre::Result<()> {
    let mut config = notify::Config::default();
    if let Some(interval) = opts.poll {
        config = config.with_poll_interval(interval);
    }

    let event_handler = EventHandler {
        handle: Handle::current(),
        ghci_sender: ghci_sender.clone(),
        shutdown: handle.clone(),
        watch: opts.watch.clone(),
        file_classifier: opts.file_classifier.clone(),
    };

    let cache = FileIdMap::new();

    // `tick_rate` defaults to 1/4 of the debounce duration.
    let tick_rate = None;

    let mut debouncer: Debouncer<T, FileIdMap> = notify_debouncer_full::new_debouncer_opt(
        opts.debounce,
        tick_rate,
        event_handler,
        cache,
        config.clone(),
    )?;

    {
        let watcher = debouncer.watcher();
        let mut watched = Vec::new();
        for path in &opts.watch {
            match watcher.watch(path.as_std_path(), RecursiveMode::Recursive) {
                Ok(()) => watched.push(path),
                Err(notify::Error {
                    kind: notify::ErrorKind::Io(e),
                    ..
                }) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::error!(
                        path = ?path.absolute(),
                        "cannot watch path because it does not exist"
                    );
                }
                Err(error) => {
                    tracing::error!(?error, path = ?path.absolute(), "cannot watch path");
                }
            }
        }
        let mut cache = debouncer.cache();
        for path in watched {
            cache.add_root(path.as_std_path(), RecursiveMode::Recursive);
        }
    }

    tracing::debug!("notify watcher started");
    let mut retry_debouncer: Option<Debouncer<T, FileIdMap>> = None;

    loop {
        tokio::select! {
            _ = handle.on_shutdown_requested() => break,
            command = command_receiver.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    WatcherCommand::SetStartupRetryFiles { files, ack } => {
                        let result = if files.is_empty() {
                            stop_debouncer(&mut retry_debouncer);
                            Ok(())
                        } else {
                            match startup_retry_debouncer::<T>(
                                ghci_sender.clone(),
                                opts.watch.clone(),
                                files,
                                opts.debounce,
                                config.clone(),
                            ) {
                                Ok(new_debouncer) => {
                                    stop_debouncer(&mut retry_debouncer);
                                    retry_debouncer = Some(new_debouncer);
                                    Ok(())
                                }
                                // Keep the previous retry watcher alive if replacing it fails.
                                Err(error) => Err(format!("{error:?}")),
                            }
                        };
                        let _ = ack.send(result);
                    }
                    WatcherCommand::ClearStartupRetryFiles { ack } => {
                        stop_debouncer(&mut retry_debouncer);
                        let _ = ack.send(());
                    }
                    WatcherCommand::ValidateSourceSnapshot { expected, ack } => {
                        let result = validate_source_snapshot(
                            &ghci_sender,
                            &opts.watch,
                            &opts.file_classifier,
                            expected,
                        )
                        .await
                        .map_err(|error| format!("{error:?}"));
                        let _ = ack.send(result);
                    }
                }
            }
        }
    }

    stop_debouncer(&mut retry_debouncer);
    block_in_place(|| debouncer.stop());
    Ok(())
}

fn stop_debouncer<T: notify::Watcher>(debouncer: &mut Option<Debouncer<T, FileIdMap>>) {
    if let Some(debouncer) = debouncer.take() {
        debouncer.stop_nonblocking();
    }
}

fn startup_retry_debouncer<T: notify::Watcher>(
    ghci_sender: mpsc::Sender<WatcherEvent>,
    watch: Vec<NormalPath>,
    files: BTreeSet<Utf8PathBuf>,
    debounce: Duration,
    config: notify::Config,
) -> eyre::Result<Debouncer<T, FileIdMap>> {
    let baseline_events = files
        .iter()
        .cloned()
        .map(FileEvent::Modify)
        .collect::<BTreeSet<_>>();
    let event_handler = StartupRetryEventHandler {
        handle: Handle::current(),
        ghci_sender,
        watch,
        files: files.clone(),
        baseline_states: file_states(&baseline_events)?,
    };
    let mut debouncer: Debouncer<T, FileIdMap> = notify_debouncer_full::new_debouncer_opt(
        debounce,
        None,
        event_handler,
        FileIdMap::new(),
        config,
    )?;
    let parents = files
        .iter()
        .filter_map(|file| file.parent().map(|parent| parent.to_owned()))
        .collect::<BTreeSet<_>>();
    {
        let watcher = debouncer.watcher();
        for parent in &parents {
            watcher.watch(parent.as_std_path(), RecursiveMode::NonRecursive)?;
        }
        let mut cache = debouncer.cache();
        for parent in parents {
            cache.add_root(parent.into_std_path_buf(), RecursiveMode::NonRecursive);
        }
    }
    tracing::debug!(?files, "Installed temporary startup-retry watches");
    Ok(debouncer)
}

struct EventHandler {
    handle: Handle,
    ghci_sender: mpsc::Sender<WatcherEvent>,
    shutdown: ShutdownHandle,
    watch: Vec<NormalPath>,
    file_classifier: FileClassifier,
}

impl EventHandler {
    async fn handle_event_async(&self, event: DebounceEventResult) {
        if let Err(err) = self.handle_event_inner(event).await {
            tracing::error!("{err:?}");
            let _ = self.shutdown.request_shutdown();
        }
    }

    async fn handle_event_inner(&self, event: DebounceEventResult) -> eyre::Result<()> {
        let events = process_debounced_events(event)?;
        let mut relevant_events = BTreeSet::new();
        for event in events {
            if self.file_classifier.is_potentially_relevant(&event)? {
                relevant_events.insert(event);
            }
        }
        send_event(&self.ghci_sender, &self.watch, relevant_events, false).await
    }
}

impl DebounceEventHandler for EventHandler {
    fn handle_event(&mut self, event: DebounceEventResult) {
        self.handle.block_on(self.handle_event_async(event))
    }
}

struct StartupRetryEventHandler {
    handle: Handle,
    ghci_sender: mpsc::Sender<WatcherEvent>,
    watch: Vec<NormalPath>,
    files: BTreeSet<Utf8PathBuf>,
    baseline_states: BTreeMap<Utf8PathBuf, FileState>,
}

impl StartupRetryEventHandler {
    async fn handle_event_async(&self, event: DebounceEventResult) {
        let result = async {
            let mut events = process_debounced_events(event)?;
            events.retain(|event| self.files.contains(event.as_path()));
            let states = file_states(&events)?;
            events.retain(|event| {
                states.get(event.as_path()) != self.baseline_states.get(event.as_path())
            });
            send_event(&self.ghci_sender, &self.watch, events, true).await
        }
        .await;
        if let Err(err) = result {
            // Retry watches are best-effort. Ordinary configured watches remain available if a
            // temporary watcher reports an error.
            tracing::warn!("Startup-retry watcher error: {err:?}");
        }
    }
}

impl DebounceEventHandler for StartupRetryEventHandler {
    fn handle_event(&mut self, event: DebounceEventResult) {
        self.handle.block_on(self.handle_event_async(event))
    }
}

fn process_debounced_events(event: DebounceEventResult) -> eyre::Result<BTreeSet<FileEvent>> {
    let events = match event {
        Ok(events) => events,
        Err(errors) => {
            let mut fatal_error = false;
            for err in errors {
                if notify_error_is_fatal(&err) {
                    fatal_error = true;
                    tracing::error!("{err}");
                } else {
                    // Some backends accept a missing root and report it asynchronously. It is
                    // nonfatal, but must remain visible at the default log level.
                    tracing::error!("{err}");
                }
            }
            return if fatal_error {
                Err(eyre!("Watching files failed"))
            } else {
                Ok(BTreeSet::new())
            };
        }
    };
    tracing::trace!(?events, "Got events");
    file_events_from_action(events)
}

async fn send_event(
    ghci_sender: &mpsc::Sender<WatcherEvent>,
    watch: &[NormalPath],
    events: BTreeSet<FileEvent>,
    startup_retry: bool,
) -> eyre::Result<()> {
    let states = file_states(&events)?;
    let source_snapshot = scan_haskell_files(watch)?;
    let haskell_files = source_snapshot.keys().cloned().collect();
    if events.is_empty() {
        tracing::debug!("No relevant file events");
    } else {
        tracing::debug!(?events, files = source_snapshot.len(), "Processed events");
        ghci_sender
            .send(WatcherEvent::Reload {
                events,
                states,
                haskell_files,
                source_snapshot,
                startup_retry,
            })
            .await?;
    }
    Ok(())
}

/// Re-scan at the publication boundary. A mismatch both suppresses the stale result and creates
/// follow-up work, independently of whether the debounced notification has arrived yet.
async fn validate_source_snapshot(
    ghci_sender: &mpsc::Sender<WatcherEvent>,
    watch: &[NormalPath],
    file_classifier: &FileClassifier,
    expected: SourceSnapshot,
) -> eyre::Result<bool> {
    let current = scan_haskell_files(watch)?;
    let changed_paths = expected
        .keys()
        .chain(current.keys())
        .filter(|path| expected.get(*path) != current.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut events = BTreeSet::new();
    for path in changed_paths {
        let event = if current.contains_key(&path) {
            FileEvent::Modify(path)
        } else {
            FileEvent::Remove(path)
        };
        if file_classifier.is_potentially_relevant(&event)? {
            events.insert(event);
        }
    }
    if events.is_empty() {
        return Ok(true);
    }
    tracing::info!(
        files = events.len(),
        "Compilation source snapshot changed; suppressing its error log and scheduling a follow-up"
    );
    send_event(ghci_sender, watch, events, false).await?;
    Ok(false)
}

/// Take a fresh content snapshot instead of relying on notification ordering. Files can
/// appear without an individual event (notably when a whole directory is created).
fn scan_haskell_files(roots: &[NormalPath]) -> eyre::Result<SourceSnapshot> {
    fn visit(path: &std::path::Path, files: &mut SourceSnapshot) -> eyre::Result<()> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                visit(&entry?.path(), files)?;
            }
        } else {
            let path = Utf8PathBuf::try_from(path.to_path_buf())?;
            if is_haskell_source_file(&path) {
                let state = FileState::capture(&path)?;
                files.insert(path, state);
            }
        }
        Ok(())
    }

    let mut files = SourceSnapshot::new();
    for root in roots {
        visit(root.as_std_path(), &mut files)?;
    }
    Ok(files)
}

fn notify_error_is_fatal(err: &notify::Error) -> bool {
    match &err.kind {
        notify::ErrorKind::Io(error) => error.kind() != std::io::ErrorKind::NotFound,
        notify::ErrorKind::PathNotFound | notify::ErrorKind::WatchNotFound => false,
        _ => true,
    }
}
