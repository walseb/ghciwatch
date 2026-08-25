//! The core [`Ghci`] session struct.

use command_group::AsyncCommandGroup;
use nix::sys::signal;
use nix::sys::signal::Signal;
use owo_colors::OwoColorize;
use owo_colors::Stream::Stdout;
use std::borrow::Borrow;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::io::IsTerminal;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tokio::io::DuplexStream;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use aho_corasick::AhoCorasick;
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use eyre::eyre;
use eyre::WrapErr;
use nix::unistd::Pid;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tracing::instrument;

mod stdin;
use stdin::GhciStdin;

mod stdout;
use stdout::GhciStdout;

mod stderr;
use stderr::GhciStderr;
pub(crate) use stderr::StderrEvent;

mod memory;
mod process;
use process::kill_process_tree;
use process::GhciProcess;

pub mod manager;

mod error_log;
use error_log::ErrorLog;

pub mod parse;
use parse::parse_eval_commands;
use parse::CompilationResult;
use parse::EvalCommand;
use parse::ShowPaths;

mod ghci_command;
pub use ghci_command::GhciCommand;

mod compilation_log;
pub use compilation_log::CompilationLog;

mod writer;
use crate::buffers::GHCI_BUFFER_CAPACITY;
pub use crate::ghci::writer::GhciWriter;
use crate::haskell_source_file::is_haskell_source_file;

mod progress_writer;

mod module_set;
pub use module_set::ModuleSet;

mod file_classifier;
pub use file_classifier::FileClassifier;
use file_classifier::ReloadActions;

mod loaded_module;
use loaded_module::LoadedModule;

use crate::aho_corasick::AhoCorasickExt;
use crate::buffers::LINE_BUFFER_CAPACITY;
use crate::cli::ExperimentalFeature;
use crate::cli::Opts;
use crate::clonable_command::ClonableCommand;
use crate::event_filter::FileEvent;
use crate::format_bulleted_list;
use crate::hooks;
use crate::hooks::HookOpts;
use crate::hooks::LifecycleEvent;
use crate::ignore::GlobMatcher;
use crate::incremental_reader::IncrementalReader;
use crate::normal_path::NormalPath;
use crate::shutdown::ShutdownHandle;
use crate::CommandExt;
use crate::StringCase;

/// Maximum time an initial compiling GHCi command may produce no `Compiling` progress before it is
/// considered wedged. Other stdout does not reset this timeout.
const COMPILATION_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(90);

/// A recovery reload gets a shorter opportunity to demonstrate compilation progress before the
/// untrustworthy session is replaced.
const RECOVERY_COMPILATION_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(40);

/// Print a conspicuous diagnostic which remains visible even when tracing is filtered out.
pub(crate) fn print_ghciwatch_error(summary: &str, details: &str) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|error| format!("unavailable ({error})"));
    println!(
        "\n==GHCIWATCH ERROR==\nTimestamp (Unix): {timestamp}\nSummary: {summary}\n{details}\n==END GHCIWATCH ERROR==\n"
    );
}

/// Marks an interruption failure after GHCi has been force-killed for recovery.
#[derive(Debug)]
pub(crate) struct GhciRecoveryFailed;

impl std::fmt::Display for GhciRecoveryFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GHCi interruption recovery failed; the session was force-killed")
    }
}

impl std::error::Error for GhciRecoveryFailed {}

/// The `ghci` prompt we use. Should be unique enough, but maybe we can make it better with Unicode
/// private-use-area codepoints or something in the future.
pub const PROMPT: &str = "###~GHCIWATCH-PROMPT~###";

/// Options for constructing a [`Ghci`]. This is like a lower-effort builder interface, mostly provided
/// because Rust tragically lacks named arguments.
///
/// Some of the other `*Opts` structs include borrowed data from the [`Opts`] struct, but this one
/// is fully owned; ultimately, this is because [`Ghci`] is run through a [`ShutdownHandle`], which
/// requires that the task is fully owned.
#[derive(Debug, Clone)]
pub struct GhciOpts {
    /// The command used to start the underlying `ghci` session.
    pub command: ClonableCommand,
    /// A path to write `ghci` errors to.
    pub error_path: Option<Utf8PathBuf>,
    /// Enable running eval commands in files.
    pub enable_eval: bool,
    /// Unix socket path used for executable eval requests.
    pub eval_socket: Utf8PathBuf,
    /// Extra directories to add to the module import search paths parsed from `:show paths`,
    /// used for converting module paths to module names and vice versa.
    pub extra_search_paths: Vec<Utf8PathBuf>,
    /// Lifecycle hooks, mostly `ghci` commands to run at certain points.
    pub hooks: HookOpts,
    /// Shell commands to run synchronously before sending SIGINT.
    pub before_interrupt: Vec<ClonableCommand>,
    /// Shell commands to run synchronously before sending SIGKILL.
    pub before_kill: Vec<ClonableCommand>,
    /// Restart the `ghci` session when paths matching these globs are changed.
    pub restart_globs: GlobMatcher,
    /// Reload the `ghci` session when paths matching these globs are changed.
    pub reload_globs: GlobMatcher,
    /// Determines whether we should interrupt a reload in progress or not.
    pub interrupt_reloads: bool,
    /// Whether watched changes automatically issue `:reload`.
    pub auto_reload: bool,
    /// Whether watched source additions and removals automatically update GHCi targets.
    pub auto_targets: bool,
    /// Whether discovering a Haskell module restarts the package-managed session.
    pub restart_on_add: bool,
    /// Whether unexpected exits keep starting delayed replacements despite an unchanged crash loop.
    pub restart_on_exit: bool,
    /// Where to write what `ghci` emits to `stdout`. Inherits parent's `stdout` by default.
    pub stdout_writer: GhciWriter,
    /// Where to write what `ghci` emits to `stderr`. Inherits parent's `stderr` by default.
    pub stderr_writer: GhciWriter,
    /// Whether to clear the screen before reloads and restarts.
    pub clear: bool,
}

impl GhciOpts {
    /// Construct options for [`Ghci`] from parsed command-line interface arguments as [`Opts`].
    ///
    /// This extracts the bits of an [`Opts`] struct relevant to the [`Ghci`] session without
    /// cloning or taking ownership of the entire thing.
    ///
    /// If running in TUI mode, `ghci` output (from `stdout_writer` and `stderr_writer`) is sent to
    /// the stream given by the second return value.
    pub fn from_cli(opts: &Opts) -> eyre::Result<(Self, Option<DuplexStream>)> {
        // TODO: implement fancier default command
        // See: https://github.com/ndmitchell/ghcid/blob/e2852979aa644c8fed92d46ab529d2c6c1c62b59/src/Ghcid.hs#L142-L171
        let command = match (&opts.file, &opts.command) {
            (Some(file), None) => ClonableCommand::new("ghci").arg(file.relative()),
            (None, Some(command)) => command.clone(),
            (None, None) => ClonableCommand::new("cabal").arg("repl"),
            (Some(_), Some(_)) => unreachable!(),
        };

        enum OutputMode {
            Tui,
            Progress,
            Standard,
        }

        let mode = if opts.has_experimental_feature(ExperimentalFeature::Tui) {
            if opts.has_experimental_feature(ExperimentalFeature::Progress) {
                tracing::warn!(
                    "`--experimental-features tui` and `--experimental-features progress` \
                     are mutually exclusive; `progress` will be ignored in TUI mode"
                );
            }
            OutputMode::Tui
        } else if opts.has_experimental_feature(ExperimentalFeature::Progress)
            && std::io::stdout().is_terminal()
        {
            OutputMode::Progress
        } else {
            OutputMode::Standard
        };

        let stdout_writer;
        let stderr_writer;
        let tui_reader;

        match mode {
            OutputMode::Tui => {
                let (tui_writer, tui_reader_inner) = tokio::io::duplex(GHCI_BUFFER_CAPACITY);
                let tui_writer = GhciWriter::duplex_stream(tui_writer);
                stdout_writer = tui_writer.clone();
                stderr_writer = tui_writer.clone();
                tui_reader = Some(tui_reader_inner);
            }
            OutputMode::Progress => {
                stdout_writer = GhciWriter::stdout().with_progress(true);
                stderr_writer = GhciWriter::stderr();
                tui_reader = None;
            }
            OutputMode::Standard => {
                stdout_writer = GhciWriter::stdout();
                stderr_writer = GhciWriter::stderr();
                tui_reader = None;
            }
        }

        Ok((
            Self {
                command,
                error_path: opts.error_file.clone(),
                enable_eval: opts.enable_eval,
                eval_socket: opts.eval_socket.clone(),
                extra_search_paths: opts
                    .extra_module_search_paths
                    .iter()
                    .map(|path| path.absolute().to_owned())
                    .collect(),
                hooks: opts.hooks.clone(),
                before_interrupt: opts.before_interrupt.clone(),
                before_kill: opts.before_kill.clone(),
                restart_globs: opts.watch.restart_globs()?,
                reload_globs: opts.watch.reload_globs()?,
                interrupt_reloads: opts.interrupt_reloads(),
                auto_reload: !opts.no_auto_reload,
                auto_targets: !opts.no_auto_targets,
                restart_on_add: opts.restart_on_add,
                restart_on_exit: opts.restart_on_exit,
                stdout_writer,
                stderr_writer,
                clear: opts.clear,
            },
            tui_reader,
        ))
    }

    /// Create a [`FileClassifier`] from these options.
    ///
    /// The classifier uses the process's current working directory. Call
    /// [`FileClassifier::set_cwd`] after GHCi initialization to update it.
    pub fn file_classifier(&self) -> eyre::Result<FileClassifier> {
        FileClassifier::new(self.restart_globs.clone(), self.reload_globs.clone())
    }

    #[instrument(skip_all, level = "trace")]
    fn clear(&self) {
        if self.clear {
            tracing::trace!("Clearing the screen");
            if let Err(err) = clearscreen::clear() {
                tracing::debug!("Failed to clear the terminal: {err}");
            }
        }
    }
}

/// A `ghci` session.
pub struct Ghci {
    /// Options used to start this `ghci` session. We keep this around so we can reuse it when
    /// restarting this session.
    opts: GhciOpts,
    /// The shutdown handle, used for performing or responding to graceful shutdowns.
    shutdown: ShutdownHandle,
    /// PID of the process created directly from the configured `--command`.
    process_id: Pid,
    /// The process group ID of the `ghci` session process.
    ///
    /// This is used to send the process tree `Ctrl-C` (`SIGINT`) to cancel reloads or other
    /// actions.
    process_group_id: Pid,
    /// The stdin writer.
    pub(crate) stdin: GhciStdin,
    /// The stdout reader.
    pub(crate) stdout: GhciStdout,
    /// Requests intentional shutdown from [`GhciProcess`]. Its acknowledgement guarantees the
    /// process tree has exited before this session drops its stdout/stderr readers.
    restart_sender: mpsc::Sender<oneshot::Sender<()>>,
    /// Sender for notifying [`run_ghci`][manager::run_ghci] when `ghci` exits unexpectedly.
    /// Cloned into each new [`GhciProcess`] on construction; kept alive here so the channel is
    /// never closed while this session is live.
    exited_sender: mpsc::Sender<ExitStatus>,
    /// Writer for `ghcid`-compatible output, useful for editor integration for diagnostics.
    error_log: ErrorLog,
    /// Classifies file events into reload actions based on glob patterns.
    classifier: FileClassifier,
    /// The set of targets for this `ghci` session, from `:show targets`.
    ///
    /// Targets that fail to compile don't show up in `:show modules` and aren't, technically
    /// speaking, loaded, but we also get an error if we `:add` them due to [GHC bug
    /// #13254][ghc-13254], so we track them here.
    ///
    /// [ghc-13254]: https://gitlab.haskell.org/ghc/ghc/-/issues/13254
    targets: ModuleSet,
    /// Last filesystem snapshot successfully applied to GHCi's target set.
    known_haskell_files: BTreeSet<NormalPath>,
    /// A replacement may return to its prompt after failing to compile. Retain that failure so
    /// the next relevant edit restarts the incomplete session even with `--no-auto-reload`.
    initialization_failure: Option<CompilationLog>,
    /// Eval commands, if `opts.enable_eval` is set.
    eval_commands: BTreeMap<NormalPath, Vec<EvalCommand>>,
    /// Search paths / current working directory for this `ghci` session.
    search_paths: ShowPaths,
    /// Tasks running `async:` shell commands in the background.
    command_handles: Vec<JoinHandle<eyre::Result<ExitStatus>>>,
    /// Monotonic counter for generating unique sync barrier nonces.
    sync_nonce: u64,
    /// Set before force-killing a session whose prompt synchronization failed.
    recovery_restart_required: bool,
}

impl Debug for Ghci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ghci")
            .field("pid", &self.process_id)
            .field("pgid", &self.process_group_id)
            .finish()
    }
}

impl Ghci {
    /// Runtime details useful when diagnosing an unexpected GHCi exit.
    pub(crate) fn diagnostic_context(&self) -> String {
        format!(
            "Component: configured GHCi --command process\nCommand process ID: {}\nProcess group ID: {}\nWorking directory: {}\nCommand: {}",
            self.process_id, self.process_group_id, self.search_paths.cwd, self.opts.command
        )
    }

    pub(crate) fn process_id(&self) -> Pid {
        self.process_id
    }

    pub(crate) fn process_group_id(&self) -> Pid {
        self.process_group_id
    }

    pub(crate) fn recovery_restart_required(&self) -> bool {
        self.recovery_restart_required
    }

    /// Absolute watched-source snapshot last synchronized with this session.
    pub(crate) fn known_haskell_files_absolute(&self) -> BTreeSet<Utf8PathBuf> {
        self.known_haskell_files
            .iter()
            .map(|path| path.absolute().to_owned())
            .collect()
    }
}

impl Ghci {
    /// Restart a leaking session through the normal lifecycle-hook and target-sync path.
    pub(crate) async fn restart_for_memory_watchdog(&mut self) -> eyre::Result<()> {
        let haskell_files = self.known_haskell_files.clone();
        self.opts.clear();
        self.restart(haskell_files).await
    }

    /// Start a replacement after failed SIGINT recovery. The old session is dead, so
    /// before-restart GHCi hooks cannot run; after hooks and watched-target sync still do.
    pub(crate) async fn restart_after_recovery_kill(
        &mut self,
        haskell_files: BTreeSet<Utf8PathBuf>,
    ) -> eyre::Result<()> {
        let haskell_files = haskell_files
            .into_iter()
            .map(|path| self.classifier.relative_path(path))
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        self.restart_after_recovery_kill_inner(haskell_files).await
    }

    pub(crate) async fn restart_after_recovery_kill_with_known_files(
        &mut self,
    ) -> eyre::Result<()> {
        let haskell_files = self.known_haskell_files.clone();
        self.restart_after_recovery_kill_inner(haskell_files).await
    }

    async fn restart_after_recovery_kill_inner(
        &mut self,
        haskell_files: BTreeSet<NormalPath>,
    ) -> eyre::Result<()> {
        let mut log = CompilationLog::default();
        self.opts.clear();
        self.restart_inner(
            &mut log,
            [
                LifecycleEvent::Startup(hooks::When::After),
                LifecycleEvent::Restart(hooks::When::After),
            ],
            Some(haskell_files),
        )
        .await
    }
}

impl Ghci {
    /// Start a new `ghci` session.
    ///
    /// This starts a number of asynchronous tasks to manage the `ghci` session's input and output
    /// streams.
    #[instrument(skip_all, level = "debug", name = "ghci")]
    pub async fn new(
        mut shutdown: ShutdownHandle,
        opts: GhciOpts,
        exited_sender: mpsc::Sender<ExitStatus>,
    ) -> eyre::Result<Self> {
        let mut command_handles = Vec::new();
        {
            let span = tracing::debug_span!("before_startup_shell");
            let _enter = span.enter();
            opts.hooks
                .run_shell_hooks(
                    LifecycleEvent::Startup(hooks::When::Before),
                    &mut command_handles,
                )
                .await?;
        }

        let mut group = {
            let mut command = opts.command.as_tokio();

            command
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true);

            command
                .group_spawn()
                .wrap_err_with(|| format!("Failed to start {}", command.display()))?
        };

        let process_group_id = Pid::from_raw(
            group
                .id()
                .ok_or_else(|| eyre!("ghci process has no process group ID"))? as i32,
        );

        let child = group.inner();
        let process_id = Pid::from_raw(
            child
                .id()
                .ok_or_else(|| eyre!("ghci process has no process ID"))? as i32,
        );
        tracing::debug!(
            pid = process_id.as_raw(),
            pgid = process_group_id.as_raw(),
            "Started ghci"
        );

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // TODO: Is this a good capacity? Maybe it should just be 1.
        let (stderr_sender, stderr_receiver) = mpsc::channel(8);

        let stdout = GhciStdout {
            reader: IncrementalReader::new(stdout).with_writer(opts.stdout_writer.clone()),
            stderr_sender: stderr_sender.clone(),
            buffer: vec![0; LINE_BUFFER_CAPACITY],
            prompt_patterns: AhoCorasick::from_anchored_patterns([PROMPT]),
            stderr_sync_nonce: 0,
        };

        let stdin = GhciStdin { stdin };

        shutdown
            .spawn("stderr", |shutdown| {
                GhciStderr {
                    shutdown,
                    reader: BufReader::new(stderr).lines(),
                    writer: opts.stderr_writer.clone(),
                    receiver: stderr_receiver,
                    buffer: String::with_capacity(LINE_BUFFER_CAPACITY),
                    forwarding: true,
                    suppressed_buffer: String::with_capacity(LINE_BUFFER_CAPACITY),
                }
                .run()
            })
            .await;

        let (restart_sender, restart_receiver) = mpsc::channel(1);

        shutdown
            .spawn("ghci_process", |shutdown| {
                GhciProcess {
                    shutdown,
                    restart_receiver,
                    process_id,
                    process_group_id,
                    exited_sender: exited_sender.clone(),
                    before_kill: opts.before_kill.clone(),
                }
                .run(group)
            })
            .await;

        let error_log = ErrorLog::new(match &opts.error_path {
            Some(error_path) => Some(NormalPath::from_cwd(error_path)?),
            None => None,
        });
        let classifier =
            FileClassifier::new(opts.restart_globs.clone(), opts.reload_globs.clone())?;
        let extra_search_paths = opts.extra_search_paths.clone();

        Ok(Ghci {
            opts,
            shutdown: shutdown.clone(),
            process_id,
            process_group_id,
            stdin,
            stdout,
            restart_sender,
            exited_sender,
            error_log,
            classifier,
            targets: Default::default(),
            known_haskell_files: Default::default(),
            initialization_failure: None,
            eval_commands: Default::default(),
            search_paths: ShowPaths {
                cwd: crate::current_dir_utf8()?,
                search_paths: extra_search_paths,
            },
            command_handles,
            sync_nonce: 0,
            recovery_restart_required: false,
        })
    }

    /// Perform post-startup initialization.
    ///
    /// Diagnostics will be added to the given `log`, and the error log will be written.
    #[instrument(level = "debug", skip_all)]
    pub async fn initialize<const N: usize>(
        &mut self,
        log: &mut CompilationLog,
        events: [LifecycleEvent; N],
        haskell_files: Option<BTreeSet<NormalPath>>,
    ) -> eyre::Result<()> {
        let start_instant = Instant::now();

        // Don't propagate the error here immediately so we can be sure we always write the
        // compilation log.
        let result = async {
            self.initialize_inner(log).await?;
            if let Some(haskell_files) = haskell_files {
                self.synchronize_haskell_files(haskell_files, log).await?;
            }
            Ok(())
        }
        .await;
        if let Err(err) = result.as_ref() {
            let failure_message = if error_is_broken_pipe(err) {
                // If the command dies before GHCi boots, no prompt exists for normal marker-based
                // stderr synchronization. The pipe has closed, so EOF is the ordering boundary.
                let startup_output = match self.stdout.drain_stderr_after_exit(log).await {
                    Ok(output) => output,
                    Err(drain_error) => {
                        tracing::debug!("Failed to collect startup stderr: {drain_error}");
                        String::new()
                    }
                };
                let startup_output = startup_output.trim();
                if startup_output.is_empty() {
                    "Configured GHCi command exited before startup completed".to_owned()
                } else {
                    format!(
                        "Configured GHCi command exited before startup completed:\n{startup_output}"
                    )
                }
            } else {
                // Initialization can also fail while the command remains alive, for example on a
                // target bookkeeping or pipe-protocol error. Do not misreport that as process exit.
                format!("GHCi initialization failed: {err:#}")
            };
            // Cabal configure/plugin failures are often plain prose rather than GHC diagnostics.
            // Preserve that output as a no-location error instead of publishing an empty success log.
            log.mark_failed_with_diagnostic(failure_message);
            // If writing the compilation log or running hooks fails, we should log this error so
            // it's not lost forever.
            tracing::debug!("Initializing failed: {err}");
        }

        // If we're in `--repl-no-load`, we may not have gotten a summary message. In that case,
        // fill in an empty "All good (0 modules)" message.
        //
        // Note: We ONLY want to do this on startup.
        log.fill_empty_summary();
        self.initialization_failure =
            matches!(log.result(), Some(CompilationResult::Err)).then(|| log.clone());
        self.finish_compilation(start_instant, log, events, result.is_ok(), result.is_ok())
            .await?;

        result
    }

    async fn initialize_inner(&mut self, log: &mut CompilationLog) -> eyre::Result<()> {
        // Wait for the stdout job to start up.
        self.stdout.initialize(log).await?;

        // Perform start-of-session initialization.
        self.stdin.initialize(&mut self.stdout, log).await?;

        // Get the initial list of targets.
        self.refresh_targets().await?;
        // Get the initial list of eval commands.
        self.refresh_eval_commands().await?;

        Ok(())
    }

    fn get_reload_actions(
        &self,
        events: BTreeSet<FileEvent>,
        haskell_files: &BTreeSet<NormalPath>,
    ) -> eyre::Result<ReloadActions> {
        // Debounced notifications can race with atomic replacement. Reconcile Haskell event kinds
        // with the newer complete snapshot so a stale Remove hint cannot unadd a file that exists
        // (or a stale Modify hint hide a deletion). Non-Haskell events still use their hints.
        let events = events
            .into_iter()
            .map(|event| {
                let normalized = self.classifier.relative_path(event.as_path())?;
                if !is_haskell_source_file(&normalized) {
                    return Ok(event);
                }
                let path = event.as_path().to_owned();
                Ok(if haskell_files.contains(&normalized) {
                    FileEvent::Modify(path)
                } else {
                    FileEvent::Remove(path)
                })
            })
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        let recovery_paths = if self.initialization_failure.is_some() {
            events
                .iter()
                .filter(|event| is_haskell_source_file(event.as_path()))
                .map(|event| self.classifier.relative_path(event.as_path()))
                .collect::<eyre::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let mut actions = self.classifier.classify(events, &self.targets)?;
        for path in recovery_paths {
            if !actions.needs_restart.contains(&path) {
                actions.needs_restart.push(path);
            }
        }
        if !self.opts.auto_reload {
            actions.needs_reload.clear();
            // Keep target additions/removals and restart-glob actions intact.
        }

        // Notification streams are hints, not transactions. Diff the complete snapshot against
        // the last accepted snapshot, then confirm the target is still absent. Rejected additions
        // are deliberately omitted from `known_haskell_files`, so later events retry them without
        // repeatedly adding command-provided targets whose Cabal-relative paths GHCi reports
        // differently from the watched path.
        for path in haskell_files.difference(&self.known_haskell_files) {
            if !self.targets.contains_source_path(path) && !actions.needs_add.contains(path) {
                actions.needs_add.push(path.clone());
            }
        }
        for path in self.known_haskell_files.difference(haskell_files) {
            if self.targets.contains_source_path(path) && !actions.needs_remove.contains(path) {
                actions.needs_remove.push(path.clone());
            }
        }
        // A path-based `:add` is correct for ordinary interpreted sessions, but in an
        // object-code Cabal session it creates an interactive-unit target rather than extending
        // the configured home unit. Symbols compiled under the package-qualified home unit can
        // then be missing or duplicated. A module name cannot repair this: the running GHCi's
        // package graph predates the new module. Restart so Cabal reconstructs that graph from
        // current package metadata, assigning every object to its proper home unit.
        if self.opts.restart_on_add && !actions.needs_add.is_empty() {
            actions.needs_restart.append(&mut actions.needs_add);
        }
        if !self.opts.auto_targets {
            actions.needs_add.clear();
            actions.needs_remove.clear();
        }
        Ok(actions)
    }

    /// Synchronize watched targets and optionally reload this `ghci` session.
    ///
    /// This may fully restart the `ghci` process.
    ///
    /// NOTE: We interrupt reloads when applicable, so this function may be canceled and dropped at
    /// any `await` point!
    #[instrument(skip_all, level = "debug")]
    pub async fn reload(
        &mut self,
        events: BTreeSet<FileEvent>,
        haskell_files: BTreeSet<Utf8PathBuf>,
        kind_sender: watch::Sender<GhciReloadKind>,
    ) -> eyre::Result<()> {
        let start_instant = Instant::now();
        let haskell_files = haskell_files
            .into_iter()
            .map(|path| self.classifier.relative_path(path))
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        let actions = self.get_reload_actions(events, &haskell_files)?;

        if actions.needs_restart() {
            let _ = kind_sender.send(GhciReloadKind::Restart);
            self.opts.clear();
            tracing::info!(
                "Restarting ghci:\n{}",
                format_bulleted_list(&actions.needs_restart)
            );
            // Carry the snapshot through the restart. A fresh GHCi initially knows
            // only the command's targets; synchronize before after-hooks run.
            self.restart(haskell_files).await?;
            return Ok(());
        }

        if !actions.needs_modify() {
            let _ = kind_sender.send(GhciReloadKind::None);
            self.known_haskell_files = haskell_files;
            self.prune_command_handles();
            return Ok(());
        }

        let mut log = CompilationLog::default();
        let mut synchronized_haskell_files = haskell_files.clone();
        self.opts.clear();

        // Once the before-hooks start, always balance them with after-hooks. A protocol or
        // bookkeeping error may make GHCi unavailable, but shell hooks can still publish the
        // completed attempt and GHCi hooks are attempted whenever the prompt remains usable.
        let reload_result: eyre::Result<()> = async {
            self.run_hooks(LifecycleEvent::Reload(hooks::When::Before), &mut log)
                .await?;
            // An interrupt decision can only cancel us after the paired before-hooks completed.
            let _ = kind_sender.send(GhciReloadKind::Reload);

            if !actions.needs_remove.is_empty() {
                tracing::info!(
                    "Removing modules from ghci:\n{}",
                    format_bulleted_list(&actions.needs_remove)
                );
                self.remove_modules(&actions.needs_remove, &mut log).await?;
            }

            if !actions.needs_add.is_empty() {
                tracing::info!(
                    "Adding modules to ghci:\n{}",
                    format_bulleted_list(&actions.needs_add)
                );
                for path in self.add_modules(&actions.needs_add, &mut log).await? {
                    // Keep a rejected target dirty so a later filesystem event retries it.
                    synchronized_haskell_files.remove(&path);
                }
            }

            // Commit only targets that GHCi actually accepted. If this future is interrupted
            // earlier, the previous snapshot remains dirty.
            self.known_haskell_files = synchronized_haskell_files;

            if !actions.needs_reload.is_empty() {
                tracing::info!(
                    "Reloading ghci:\n{}",
                    format_bulleted_list(&actions.needs_reload)
                );
                // Like `:unadd`, an ordinary reload can wedge inside GHC. It is not an executable
                // eval, so monitor the output read itself and interrupt only if compilation goes quiet.
                let completed = self
                    .stdin
                    .reload(&mut self.stdout, &mut log, COMPILATION_INACTIVITY_TIMEOUT)
                    .await?;
                if completed {
                    self.refresh_eval_commands_for_paths(&actions.needs_reload)
                        .await?;
                } else {
                    print_ghciwatch_error(
                        "GHCi reload stopped reporting compilation progress",
                        &format!(
                            "Component: reload (:reload)\nInactivity timeout: {COMPILATION_INACTIVITY_TIMEOUT:?}\nProcess group ID: {}\nWorking directory: {}\nCommand: {}\nChanged paths:\n{}\nRecovery: interrupting GHCi with process-group SIGINT, then retrying :reload with a {RECOVERY_COMPILATION_INACTIVITY_TIMEOUT:?} inactivity timeout",
                            self.process_group_id,
                            self.search_paths.cwd,
                            self.opts.command,
                            format_bulleted_list(&actions.needs_reload),
                        ),
                    );
                    tracing::warn!(
                        "GHCi reported no reload compilation progress for {COMPILATION_INACTIVITY_TIMEOUT:?}; interrupting reload"
                    );
                    self.interrupt_and_retry_reload(&mut log, "reload (:reload)")
                        .await
                        .wrap_err("Failed to recover from inactive reload")?;
                    self.refresh_eval_commands_for_paths(&actions.needs_reload)
                        .await?;
                }
            }
            Ok(())
        }
        .await;

        // Target synchronization/compilation is over. Do not let a later filesystem event
        // interrupt error publication or after-reload hooks; queue it for the next dispatch.
        let _ = kind_sender.send(GhciReloadKind::None);

        let ghci_available = match &reload_result {
            Ok(()) => true,
            Err(err) => !error_is_broken_pipe(err) && !self.recovery_restart_required,
        };
        let operation_succeeded = reload_result.is_ok();
        let finish_result = self
            .finish_compilation(
                start_instant,
                &mut log,
                [LifecycleEvent::Reload(hooks::When::After)],
                ghci_available,
                operation_succeeded,
            )
            .await;

        self.prune_command_handles();

        // Prefer the error that interrupted the operation, but never return it until after-hooks
        // have had their chance to publish completion.
        reload_result?;
        finish_result?;

        Ok(())
    }

    /// Balance a reload attempt cancelled by a newer filesystem event.
    pub(crate) async fn finish_interrupted_reload(
        &mut self,
        ghci_available: bool,
    ) -> eyre::Result<()> {
        let event = LifecycleEvent::Reload(hooks::When::After);
        let mut log = CompilationLog::default();
        let result = if ghci_available {
            self.run_hooks(event, &mut log).await
        } else {
            self.opts
                .hooks
                .run_shell_hooks(event, &mut self.command_handles)
                .await
        };
        self.prune_command_handles();
        result
    }

    /// Restart the `ghci` session after an unsuccessful startup attempt.
    ///
    /// There is no GHCi prompt during an early startup failure, so only shell reload/restart hooks
    /// can run. They still bracket the replacement attempt like hooks around an ordinary restart.
    #[instrument(skip_all, level = "debug")]
    async fn startup_restart(
        &mut self,
        haskell_files: BTreeSet<Utf8PathBuf>,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        let haskell_files = haskell_files
            .into_iter()
            .map(|path| self.classifier.relative_path(path))
            .collect::<eyre::Result<BTreeSet<_>>>()?;

        self.opts
            .hooks
            .run_shell_hooks(
                LifecycleEvent::Reload(hooks::When::Before),
                &mut self.command_handles,
            )
            .await?;
        self.opts
            .hooks
            .run_shell_hooks(
                LifecycleEvent::Restart(hooks::When::Before),
                &mut self.command_handles,
            )
            .await?;

        // The previous command process has already exited, so its process watcher cannot
        // acknowledge `stop()`. Replace it directly, then let initialization drain diagnostics and
        // run the shell-only after-hooks if this attempt also exits before GHCi boots.
        let new = match Self::new(
            self.shutdown.clone(),
            self.opts.clone(),
            self.exited_sender.clone(),
        )
        .await
        {
            Ok(new) => new,
            Err(err) => {
                log.mark_failed_with_diagnostic(format!(
                    "Failed to start configured GHCi command: {err:#}"
                ));
                self.finish_compilation(
                    Instant::now(),
                    log,
                    [
                        LifecycleEvent::Startup(hooks::When::After),
                        LifecycleEvent::Restart(hooks::When::After),
                        LifecycleEvent::Reload(hooks::When::After),
                    ],
                    false,
                    false,
                )
                .await?;
                return Err(err);
            }
        };
        let _ = std::mem::replace(self, new);
        self.initialize(
            log,
            [
                LifecycleEvent::Startup(hooks::When::After),
                LifecycleEvent::Restart(hooks::When::After),
                LifecycleEvent::Reload(hooks::When::After),
            ],
            Some(haskell_files),
        )
        .await?;

        Ok(())
    }

    /// Restart the `ghci` session and synchronize its target set.
    ///
    /// A restart is also a reload attempt from the watcher's perspective, so reload hooks bracket
    /// it as well as restart hooks. This is especially important when command startup/compilation
    /// fails: external after-reload hooks still need to publish completion of the attempt.
    #[instrument(skip_all, level = "debug")]
    async fn restart(&mut self, haskell_files: BTreeSet<NormalPath>) -> eyre::Result<()> {
        let mut log = CompilationLog::default();

        if let Err(err) = self
            .run_hooks(LifecycleEvent::Reload(hooks::When::Before), &mut log)
            .await
        {
            self.run_shell_lifecycle_events([LifecycleEvent::Reload(hooks::When::After)])
                .await?;
            return Err(err);
        }
        if let Err(err) = self
            .run_hooks(LifecycleEvent::Restart(hooks::When::Before), &mut log)
            .await
        {
            self.run_shell_lifecycle_events([
                LifecycleEvent::Restart(hooks::When::After),
                LifecycleEvent::Reload(hooks::When::After),
            ])
            .await?;
            return Err(err);
        }
        self.restart_inner(
            &mut log,
            [
                LifecycleEvent::Startup(hooks::When::After),
                LifecycleEvent::Restart(hooks::When::After),
                LifecycleEvent::Reload(hooks::When::After),
            ],
            Some(haskell_files),
        )
        .await?;

        Ok(())
    }

    #[instrument(skip_all, level = "debug")]
    async fn restart_inner<const N: usize>(
        &mut self,
        log: &mut CompilationLog,
        events: [LifecycleEvent; N],
        haskell_files: Option<BTreeSet<NormalPath>>,
    ) -> eyre::Result<()> {
        if let Err(err) = self.stop().await {
            self.run_shell_lifecycle_events(events).await?;
            return Err(err);
        }
        let new = match Self::new(
            self.shutdown.clone(),
            self.opts.clone(),
            self.exited_sender.clone(),
        )
        .await
        {
            Ok(new) => new,
            Err(err) => {
                // A spawn/setup failure has no new prompt, but still publishes a failed compilation
                // and closes every lifecycle attempt using shell hooks.
                log.mark_failed_with_diagnostic(format!(
                    "Failed to start configured GHCi command: {err:#}"
                ));
                self.finish_compilation(Instant::now(), log, events, false, false)
                    .await?;
                return Err(err);
            }
        };
        let _ = std::mem::replace(self, new);
        // `initialize` itself balances all events on both compilation and protocol failures.
        self.initialize(log, events, haskell_files).await?;

        Ok(())
    }

    /// Synchronize a complete filesystem snapshot with GHCi's target set.
    async fn synchronize_haskell_files(
        &mut self,
        haskell_files: BTreeSet<NormalPath>,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        let needs_remove = self
            .known_haskell_files
            .difference(&haskell_files)
            .filter(|path| self.targets.contains_source_path(*path))
            .cloned()
            .collect::<Vec<_>>();
        // This may be a fresh replacement session, so compare every watched source with the actual
        // target set rather than relying on bookkeeping inherited from the previous process.
        let needs_add = haskell_files
            .iter()
            .filter(|path| !self.targets.contains_source_path(*path))
            .cloned()
            .collect::<Vec<_>>();

        if !needs_remove.is_empty() {
            self.remove_modules(&needs_remove, log).await?;
        }
        let unresolved_adds = if needs_add.is_empty() {
            Vec::new()
        } else {
            self.add_modules(&needs_add, log).await?
        };
        self.known_haskell_files = haskell_files
            .into_iter()
            .filter(|path| !unresolved_adds.contains(path))
            .collect();
        Ok(())
    }

    /// Run the user provided test command.
    #[instrument(skip_all, level = "debug")]
    async fn test(&mut self, log: &mut CompilationLog) -> eyre::Result<()> {
        self.run_hooks(LifecycleEvent::Test, log).await?;
        Ok(())
    }

    /// Run the eval commands, if enabled.
    #[instrument(skip_all, level = "debug")]
    async fn eval(&mut self, log: &mut CompilationLog) -> eyre::Result<()> {
        if !self.opts.enable_eval {
            return Ok(());
        }

        // TODO: This `clone` is ugly but I can't get the borrow checker to accept it otherwise.
        // Might be more efficient to swap it out for a default, but then it gets trickier to
        // restore the old value when the function returns.
        for (path, commands) in self.eval_commands.clone() {
            // If we don't have any eval commands for this path, do nothing.
            if commands.is_empty() {
                continue;
            }

            // If the `module` was already compiled, `ghci` may have loaded the interface file instead
            // of the interpreted bytecode, giving us this error message when we attempt to
            // load the top-level scope with `:module + *{module}`:
            //
            //     module 'Mercury.Typescript.Golden' is not interpreted
            //
            // We use `:add *{module}` to force interpreting the module. We do this here instead of in
            // `add_module` to save time if eval commands aren't used (or aren't needed for a
            // particular module).
            tracing::info!("Loading {path} in interpreted mode for eval commands");
            self.interpret_module(&path, log).await?;
            let module = self.search_paths.path_to_module(&path)?;
            self.stdin
                .add_module_to_scope(&mut self.stdout, &module, log)
                .await?;
            for command in commands {
                tracing::info!("Eval {path}:{command}");
                self.stdin
                    .run_command(&mut self.stdout, &command.command, log)
                    .await?;
            }
            self.stdin
                .remove_module_from_scope(&mut self.stdout, &module, log)
                .await?;
        }

        Ok(())
    }

    /// Refresh the listing of targets by parsing the `:show paths` and `:show targets` output.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_targets(&mut self) -> eyre::Result<()> {
        self.refresh_paths().await?;
        self.targets = self
            .stdin
            .show_targets(&mut self.stdout, &self.search_paths)
            .await?;
        tracing::debug!(targets = self.targets.len(), "Parsed targets");
        Ok(())
    }

    /// Refresh the listing of search paths by parsing the `:show paths` output.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_paths(&mut self) -> eyre::Result<()> {
        self.search_paths = self.stdin.show_paths(&mut self.stdout).await?;
        for path in &self.opts.extra_search_paths {
            if !self.search_paths.search_paths.contains(path) {
                self.search_paths.search_paths.push(path.clone());
            }
        }
        self.classifier.set_cwd(self.search_paths.cwd.clone());
        tracing::debug!(cwd = %self.search_paths.cwd, search_paths = ?self.search_paths.search_paths, "Parsed paths");
        Ok(())
    }

    /// Refresh `eval_commands` by reading and parsing the files in `targets`.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_eval_commands(&mut self) -> eyre::Result<()> {
        if !self.opts.enable_eval {
            return Ok(());
        }

        let mut eval_commands = BTreeMap::new();

        for target in self.targets.iter() {
            // Note: Loaded targets are always Haskell modules.
            let commands = Self::parse_eval_commands(target.path()).await?;
            if !commands.is_empty() {
                eval_commands.insert(target.path().clone(), commands);
            }
        }

        self.eval_commands = eval_commands;
        Ok(())
    }

    /// Refresh `eval_commands` by reading and parsing the given files.
    #[instrument(skip_all, level = "debug")]
    async fn refresh_eval_commands_for_paths(
        &mut self,
        paths: impl IntoIterator<Item = impl Borrow<NormalPath>>,
    ) -> eyre::Result<()> {
        if !self.opts.enable_eval {
            return Ok(());
        }

        for path in paths {
            let path = path.borrow();

            // To actually _execute_ eval commands with the proper bindings in scope, we need to be
            // able to evaluate (interpret) a file, which requires we know its module _name_
            // (because `:module + *MODULE_NAME` only supports module names and not source paths).
            //
            // We get _all_ file events in this loop, not just Haskell source files, so let's guard
            // adding an entry to the `eval_commands` map by making sure we can convert the path to
            // a module name.
            //
            // However!!! We're _modifying_ an existing map here, so if we look at a path and
            // _don't_ find any commands, we need to be careful to _remove_ that entry from the map.
            //
            // Hey maybe this should just be a generic multimap structure, anyone ever think of that?
            if self.search_paths.path_to_module(path).is_err() {
                if is_haskell_source_file(path) {
                    // If the path is a Haskell source file (ends with `.hs` or similar), we should
                    // warn the user directly. Otherwise, it's probably a `.persistentmodels` or
                    // something and the user (probably!) won't expect eval commands to be evaluated
                    // in it.
                    tracing::warn!(%path, "Could not determine module path, skipping parsing eval commands");
                } else {
                    tracing::debug!(%path, "Could not determine module path, skipping parsing eval commands");
                }
                self.eval_commands.remove(path);
                continue;
            }

            let commands = Self::parse_eval_commands(path).await?;
            if commands.is_empty() {
                self.eval_commands.remove(path);
            } else {
                self.eval_commands.insert(path.clone(), commands);
            }
        }

        Ok(())
    }

    /// Remove all `eval_commands` for the given paths.
    #[instrument(skip_all, level = "debug")]
    async fn clear_eval_commands_for_paths(
        &mut self,
        paths: impl IntoIterator<Item = impl Borrow<NormalPath>>,
    ) {
        if !self.opts.enable_eval {
            return;
        }

        for path in paths {
            self.eval_commands.remove(path.borrow());
        }
    }

    /// Read and parse eval commands from the given `path`.
    #[instrument(level = "trace")]
    async fn parse_eval_commands(path: &Utf8Path) -> eyre::Result<Vec<EvalCommand>> {
        let contents = tokio::fs::read_to_string(path)
            .await
            .wrap_err_with(|| format!("Failed to read {path}"))?;
        let commands = parse_eval_commands(&contents)
            .wrap_err_with(|| format!("Failed to parse eval commands from file {path}"))?;
        Ok(commands)
    }

    /// `:add` a module or modules to the GHCi session.
    ///
    /// Returns paths which GHCi did not accept into its target set.
    #[instrument(skip(self), level = "debug")]
    async fn add_modules(
        &mut self,
        paths: &[NormalPath],
        log: &mut CompilationLog,
    ) -> eyre::Result<Vec<NormalPath>> {
        for path in paths {
            if self.targets.contains_source_path(path) {
                return Err(eyre!(
                    "Attempting to add already-loaded module: {path}\n\
                     This is a ghciwatch bug; please report it upstream"
                ));
            }
        }

        // Prefer module names when GHCi's current package graph can resolve them. This keeps Cabal
        // home-unit modules in their configured unit instead of creating duplicate interactive-unit
        // targets. A genuinely new module is often absent from that graph, so refresh the targets
        // and retry every unresolved name by source path.
        let named_modules = paths
            .iter()
            .filter_map(|path| {
                self.search_paths
                    .path_to_module(path)
                    .ok()
                    .map(|name| LoadedModule::with_name(path.clone(), name))
            })
            .collect::<Vec<_>>();
        let mut named_log = CompilationLog::default();
        if !named_modules.is_empty() {
            let named_paths = named_modules
                .iter()
                .map(|module| module.path().clone())
                .collect::<Vec<_>>();

            // Name lookup is intentionally speculative: flattened `:show paths` output can derive
            // a valid name which is not resolvable from GHCi's current home unit. Keep expected
            // `cannot be found locally` diagnostics out of user output. If every name enters the
            // target set, replay stderr because it may contain authoritative warnings or source
            // errors. Otherwise the path fallback recompiles the complete target set and supplies
            // the visible diagnostics.
            self.stdout.set_stderr_forwarding(false, false).await?;
            let named_result: eyre::Result<()> = async {
                self.add_loaded_modules(&named_modules, &named_paths, &mut named_log)
                    .await?;
                self.refresh_targets().await
            }
            .await;
            if let Err(err) = named_result {
                // Preserve unexpected/protocol failure output before returning the failure.
                self.stdout.set_stderr_forwarding(true, true).await?;
                return Err(err);
            }
            let all_names_resolved = named_paths
                .iter()
                .all(|path| self.targets.contains_source_path(path));
            self.stdout
                .set_stderr_forwarding(true, all_names_resolved)
                .await?;
        }

        let unresolved_after_names = paths
            .iter()
            .filter(|path| !self.targets.contains_source_path(*path))
            .cloned()
            .collect::<Vec<_>>();
        if unresolved_after_names.is_empty() {
            // The named attempt is the final compilation result, including legitimate source
            // diagnostics from modules which entered the target set but failed to compile.
            log.diagnostics.append(&mut named_log.diagnostics);
            if named_log.summary.is_some() {
                log.summary = named_log.summary;
            }
        } else {
            tracing::debug!(
                "Retrying modules unresolved by named :add as source paths:\n{}",
                format_bulleted_list(&unresolved_after_names)
            );
            let path_modules = unresolved_after_names
                .iter()
                .cloned()
                .map(LoadedModule::new)
                .collect::<Vec<_>>();
            // Discard lookup diagnostics from the speculative named attempt. The path command
            // recompiles the complete target set and supplies the authoritative final log.
            self.add_loaded_modules(&path_modules, &unresolved_after_names, log)
                .await?;
            self.refresh_targets().await?;
        }

        let unresolved_paths = paths
            .iter()
            .filter(|path| !self.targets.contains_source_path(*path))
            .cloned()
            .collect::<Vec<_>>();
        if unresolved_paths.is_empty() {
            // GHCi command errors do not necessarily emit a compilation summary. Do not let an
            // earlier successful operation's summary make this addition look successful.
            log.fill_empty_summary();
        } else {
            tracing::warn!(
                "GHCi did not add some modules:\n{}",
                format_bulleted_list(&unresolved_paths)
            );
            log.mark_failed();
        }

        let accepted_paths = paths
            .iter()
            .filter(|path| self.targets.contains_source_path(*path))
            .cloned()
            .collect::<Vec<_>>();
        self.refresh_eval_commands_for_paths(accepted_paths).await?;

        Ok(unresolved_paths)
    }

    /// Submit one monitored `:add` command.
    async fn add_loaded_modules(
        &mut self,
        modules: &[LoadedModule],
        paths: &[NormalPath],
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        // Like `:reload` and `:unadd`, `:add` compiles the target set and can wedge inside GHC.
        // Monitor stdout activity rather than imposing an absolute compilation duration.
        let completed = self
            .stdin
            .add_modules(
                &mut self.stdout,
                modules,
                log,
                COMPILATION_INACTIVITY_TIMEOUT,
            )
            .await?;
        if !completed {
            print_ghciwatch_error(
                "GHCi module addition stopped reporting compilation progress",
                &format!(
                    "Component: target synchronization (:add)\nInactivity timeout: {COMPILATION_INACTIVITY_TIMEOUT:?}\nProcess group ID: {}\nWorking directory: {}\nCommand: {}\nAdded paths:\n{}\nRecovery: interrupting GHCi with process-group SIGINT, then retrying :reload with a {RECOVERY_COMPILATION_INACTIVITY_TIMEOUT:?} inactivity timeout",
                    self.process_group_id,
                    self.search_paths.cwd,
                    self.opts.command,
                    format_bulleted_list(paths),
                ),
            );
            tracing::warn!(
                "GHCi reported no :add compilation progress for {COMPILATION_INACTIVITY_TIMEOUT:?}; interrupting module compilation"
            );
            self.interrupt_and_retry_reload(log, "target synchronization (:add)")
                .await
                .wrap_err("Failed to recover from inactive :add")?;
        }

        Ok(())
    }

    /// `:add *` a module to the `ghci` session by path.
    ///
    /// This forces it to be interpreted.
    #[instrument(skip(self), level = "debug")]
    async fn interpret_module(
        &mut self,
        path: &NormalPath,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        let module = self.targets.get_import_name(path);

        self.stdin
            .interpret_module(&mut self.stdout, &module, log)
            .await?;

        // Note: A borrowed path is only returned if the path is already present in the module set.
        if let Cow::Owned(module) = module {
            self.targets.insert_module(module);
        }

        self.refresh_eval_commands_for_paths(std::iter::once(path))
            .await?;

        Ok(())
    }

    /// `:unadd` a module or modules from the `ghci` session by path.
    #[instrument(skip(self), level = "debug")]
    async fn remove_modules(
        &mut self,
        paths: &[NormalPath],
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        let modules = paths
            .iter()
            .map(|path| self.targets.get_import_name(path).into_owned())
            .collect::<Vec<_>>();

        // Each `:unadd` implicitly reloads as well, so we have to `:unadd` all the modules in a
        // single command so that GHCi doesn't try to load a bunch of removed modules after each
        // one.
        //
        // GHCi can occasionally wedge in that implicit reload. The executable-eval timeout cannot
        // help here because reload owns the eval barrier and mutex, so monitor output inactivity.
        // GHCi updates its target set before starting the implicit reload; after recovery it is safe
        // to update our target bookkeeping below.
        let completed = self
            .stdin
            .remove_modules(
                &mut self.stdout,
                modules.iter().map(Borrow::borrow),
                log,
                COMPILATION_INACTIVITY_TIMEOUT,
            )
            .await?;
        if !completed {
            print_ghciwatch_error(
                "GHCi module removal stopped reporting compilation progress",
                &format!(
                    "Component: target synchronization (:unadd, including its implicit reload)\nInactivity timeout: {COMPILATION_INACTIVITY_TIMEOUT:?}\nProcess group ID: {}\nWorking directory: {}\nCommand: {}\nRemoved paths:\n{}\nRecovery: interrupting GHCi with process-group SIGINT, then retrying :reload with a {RECOVERY_COMPILATION_INACTIVITY_TIMEOUT:?} inactivity timeout",
                    self.process_group_id,
                    self.search_paths.cwd,
                    self.opts.command,
                    format_bulleted_list(paths),
                ),
            );
            tracing::warn!(
                "GHCi reported no :unadd compilation progress for {COMPILATION_INACTIVITY_TIMEOUT:?}; interrupting its implicit reload"
            );
            self.interrupt_and_retry_reload(
                log,
                "target synchronization (:unadd, including its implicit reload)",
            )
            .await
            .wrap_err("Failed to recover from inactive :unadd")?;
        }
        for path in paths {
            self.targets.remove_source_path(path);
        }

        self.clear_eval_commands_for_paths(paths).await;

        Ok(())
    }

    /// Interrupt an inactive compiling command and retry the target-set compilation with `:reload`.
    /// If the recovery reload also becomes inactive, kill the process tree and let the manager
    /// immediately initialize a fresh session.
    async fn interrupt_and_retry_reload(
        &mut self,
        log: &mut CompilationLog,
        inactive_component: &str,
    ) -> eyre::Result<()> {
        self.send_sigint().await?;

        tracing::warn!(
            component = inactive_component,
            "Retrying :reload after interrupting inactive GHCi compilation"
        );
        let completed = self
            .stdin
            .reload(
                &mut self.stdout,
                log,
                RECOVERY_COMPILATION_INACTIVITY_TIMEOUT,
            )
            .await?;
        if completed {
            tracing::info!(
                component = inactive_component,
                "Recovery :reload reached the GHCi prompt"
            );
            return Ok(());
        }

        print_ghciwatch_error(
            "GHCi recovery reload stopped reporting compilation progress",
            &format!(
                "Component: recovery :reload after {inactive_component}\nInactivity timeout: {RECOVERY_COMPILATION_INACTIVITY_TIMEOUT:?}\nProcess group ID: {}\nWorking directory: {}\nCommand: {}\nRecovery: force-killing the GHCi process tree; the manager will immediately initialize a fresh session",
                self.process_group_id, self.search_paths.cwd, self.opts.command,
            ),
        );
        tracing::error!(
            component = inactive_component,
            "Recovery :reload reported no compilation progress for {RECOVERY_COMPILATION_INACTIVITY_TIMEOUT:?}; force-killing GHCi for restart"
        );
        self.force_kill_for_recovery(eyre!(
            "Recovery :reload after {inactive_component} stopped reporting compilation progress for {RECOVERY_COMPILATION_INACTIVITY_TIMEOUT:?}"
        ))
        .await
    }

    /// Stop this `ghci` session and cancel the async tasks associated with it.
    #[instrument(skip_all, level = "debug")]
    async fn stop(&mut self) -> eyre::Result<()> {
        // Do not replace `self` until the old process exits: dropping its readers early makes
        // shutdown-time output fail with misleading `hPutChar: resource vanished (Broken pipe)`.
        let (ack, done) = oneshot::channel();
        if let Err(err) = self.restart_sender.send(ack).await {
            // On global shutdown the process watcher receives the same broadcast and may win the
            // race to stop the process before the manager sends this explicit request.
            if self.shutdown.error_if_shutdown_requested().is_err() {
                return Ok(());
            }
            return Err(err).wrap_err("GHCi process watcher stopped before intentional shutdown");
        }
        if let Err(err) = done.await {
            if self.shutdown.error_if_shutdown_requested().is_err() {
                return Ok(());
            }
            return Err(err)
                .wrap_err("GHCi process watcher dropped intentional shutdown acknowledgement");
        }
        Ok(())
    }

    /// Interrupt the running GHCi session.
    ///
    /// On `Err`, this method attempts to force-kill GHCi because its prompt state cannot be
    /// trusted. Callers MUST consume the process-exit notification and start a fresh session when
    /// the kill succeeds, rather than propagating the recovery error as fatal.
    #[instrument(skip_all, level = "debug")]
    pub(crate) async fn send_sigint(&mut self) -> eyre::Result<()> {
        match self.send_sigint_inner().await {
            Ok(()) => Ok(()),
            Err(error) => self.force_kill_for_recovery(error).await,
        }
    }

    /// Force-kill a session whose pipe protocol cannot be trusted and mark it for immediate
    /// replacement by the manager.
    async fn force_kill_for_recovery(&mut self, error: eyre::Report) -> eyre::Result<()> {
        // No caller may continue using a session whose prompt synchronization is uncertain.
        // Set the flag before signaling so the process-exit branch cannot observe stale state.
        self.recovery_restart_required = true;
        process::run_before_signal_commands(
            &self.opts.before_kill,
            self.process_id,
            self.process_group_id,
            "kill",
        )
        .await;
        match kill_process_tree(self.process_id, self.process_group_id) {
            Ok(()) => {}
            Err(kill_error) => {
                return Err(kill_error)
                    .wrap_err("Failed to kill GHCi process tree after unsuccessful recovery")
                    .wrap_err(format!("Original recovery error: {error:#}"))
                    .wrap_err(GhciRecoveryFailed);
            }
        }
        Err(error).wrap_err(GhciRecoveryFailed)
    }

    async fn send_sigint_inner(&mut self) -> eyre::Result<()> {
        // A cancelled speculative named `:add` may have disabled stderr forwarding. SIGINT is the
        // cancellation recovery boundary, so restore normal forwarding before doing anything else.
        self.stdout.set_stderr_forwarding(true, false).await?;
        let start_instant = Instant::now();
        process::run_before_signal_commands(
            &self.opts.before_interrupt,
            self.process_id,
            self.process_group_id,
            "interrupt",
        )
        .await;

        // Phase 1: Send SIGINT repeatedly until we find a clean, uninterrupted prompt.
        //
        // An interrupted reload can cause interleaved output between the GHCi prompt and
        // compilation output (due to GHC bug where the logging thread isn't stopped on
        // async exception — see `runParPipelines` in GHC's Driver/Make.hs). We send
        // SIGINT with exponential backoff until we see a prompt that isn't garbled.
        let mut backoff = ExponentialBackoff {
            initial_interval: Duration::from_millis(5),
            max_interval: Duration::from_millis(100),
            multiplier: 1.25,
            max_elapsed_time: Some(Duration::from_secs(10)),
            ..Default::default()
        };

        let mut sigint_count: usize = 0;
        loop {
            let Some(delay) = backoff.next_backoff() else {
                return Err(eyre!(
                    "Timed out waiting for GHCi to respond to SIGINT after {:.2?}",
                    start_instant.elapsed()
                ));
            };

            sigint_count += 1;
            signal::killpg(self.process_group_id, Signal::SIGINT)
                .wrap_err("Failed to send `Ctrl-C` (`SIGINT`) to ghci session")?;
            tracing::debug!(count = sigint_count, "Sent SIGINT");

            let found = self.stdout.buffer_and_drain_prompts(delay).await?;
            if found > 0 {
                tracing::debug!(
                    found,
                    elapsed = ?start_instant.elapsed(),
                    "Found prompt after SIGINT"
                );
                break;
            }
        }

        // If we only sent 1 SIGINT, then there cannot be extra prompts waiting to be read from the
        // buffer; only do the sync barrier process if we sent multiple SIGINTs.
        if sigint_count > 1 {
            self.sync_barrier().await?;
        }

        tracing::info!("Interrupted ghci in {:.2?}", start_instant.elapsed());
        Ok(())
    }

    /// Sync barrier: deterministically consume all stale prompts from the pipe.
    ///
    /// We rely on the fact that GHCi processes input commands one at a time, in order. When we send
    /// a command to GHCi, we read its output up until the next prompt and know that the output
    /// we've read matches the command we sent. This is important because we parse GHCi output in
    /// several places (e.g. compilation errors go to the `error_log`, `:show paths` and `:show
    /// targets` are used to inform module additions/removals/reloads, etc.), so if we're parsing
    /// output from a different command, we'll Have Problems.
    ///
    /// When we're hitting Ctrl-C repeatedly (in case of a user input prompt interleaved with
    /// compilation output in GHCi's stdout stream), we don't know how many times GHCi will print a
    /// prompt that we can read.
    ///
    /// Therefore, we _change_ the prompt and read until _that_ specific prompt shows up in the
    /// output, using a unique (to the `ghci` process) and different prompt each time we call this
    /// method. This ensures we consume all remaining stale output, without having to wait until we
    /// "think it's safe" and wasting the user's time after GHCi is done writing.
    #[instrument(skip_all, level = "debug")]
    async fn sync_barrier(&mut self) -> eyre::Result<()> {
        self.sync_nonce += 1;
        let nonce = self.sync_nonce;
        let sync_marker = format!("~~~GHCIWATCH-SYNC-{nonce}~~~");

        // Set the prompt to our sync marker.
        self.stdin
            .write_set_prompt(&sync_marker)
            .await
            .wrap_err("Failed to write sync command to ghci stdin")?;

        // From here until the prompt is restored, any failure leaves the session
        // unable to match `PROMPT` again. Restoring in-band after a failed read
        // is not safe, so return the error to `send_sigint`; its outer recovery
        // boundary marks and force-kills the session before notifying the manager.
        let sync_timeout = Duration::from_secs(3);
        let read =
            tokio::time::timeout(sync_timeout, self.stdout.read_until_marker(&sync_marker)).await;
        let result = match read {
            Ok(Ok(_ghci_output)) => self
                .stdin
                .set_prompt(
                    &mut self.stdout,
                    PROMPT,
                    crate::incremental_reader::FindAt::LineStart,
                    // We don't expect to see any compilation here, so we pass a stub
                    // `CompilationLog` and discard it.
                    &mut Default::default(),
                )
                .await
                .wrap_err("Failed to restore prompt after sync barrier"),
            Ok(Err(e)) => Err(e).wrap_err("Failed to read until sync marker"),
            Err(_elapsed) => Err(eyre!(
                "Timed out waiting for GHCi sync marker after {sync_timeout:?}"
            )),
        };

        result.wrap_err("ghci sync barrier failed because the prompt could not be restored")?;
        Ok(())
    }

    #[allow(dead_code)] // TODO: No it should not be!
    #[instrument(skip_all, level = "trace")]
    async fn before_startup_shell(command: &ClonableCommand) -> eyre::Result<()> {
        let program = &command.program;
        let mut command = command.as_tokio();
        command.kill_on_drop(true);
        let command_formatted = command.display();
        tracing::info!("$ {command_formatted}");
        let status = command
            .status()
            .await
            .wrap_err_with(|| format!("Failed to execute `{command_formatted}`"))?;
        if status.success() {
            tracing::debug!("{program:?} exited successfully: {status}");
        } else {
            tracing::error!("{program:?} failed: {status}");
        }
        Ok(())
    }

    // Get rid of any handles for background commands that have finished.
    #[instrument(skip_all, level = "trace")]
    fn prune_command_handles(&mut self) {
        self.command_handles.retain(|handle| !handle.is_finished());
    }

    /// Finish a compilation process.
    ///
    /// This outputs how long the compilation took (since `compilation_start`), runs eval and test
    /// commands (if compilation succeeded), and writes the error log.
    #[instrument(skip_all, level = "trace")]
    async fn finish_compilation<const N: usize>(
        &mut self,
        compilation_start: Instant,
        log: &mut CompilationLog,
        events: [LifecycleEvent; N],
        ghci_available: bool,
        operation_succeeded: bool,
    ) -> eyre::Result<()> {
        // Target synchronization followed by `:reload` may compile the same failure twice. Publish
        // each distinct diagnostic once while retaining errors unique to either operation.
        log.deduplicate_diagnostics();
        // Allow hooks to consume the error log by updating it before running the hooks. A failure
        // to publish the log must not suppress lifecycle completion notifications.
        let error_log_result = self.write_error_log(log).await;

        let mut hooks_have_ghci = ghci_available;
        let mut hook_result = Ok(());
        for event in events {
            let result = if hooks_have_ghci {
                // Lifecycle hooks describe the attempt, not only successful compilation. GHCi may
                // have unloaded modules after a failure, but hook diagnostics are isolated by
                // `run_hooks`, so unavailable module-based hooks do not suppress remaining hooks.
                self.run_hooks(event, log).await
            } else {
                // The configured command can fail before GHCi provides a prompt (for example while
                // Cabal builds a plugin). Shell hooks still apply, but GHCi hooks cannot be sent.
                self.opts
                    .hooks
                    .run_shell_hooks(event, &mut self.command_handles)
                    .await
            };
            if let Err(err) = result {
                // Continue with shell hooks for later lifecycle events even if the prompt failed
                // while running a GHCi hook for an earlier event.
                hooks_have_ghci = false;
                if hook_result.is_ok() {
                    hook_result = Err(err);
                }
            }
        }

        let event = events[N - 1];
        let compilation_succeeded = operation_succeeded
            && ghci_available
            && !matches!(log.result(), Some(CompilationResult::Err));

        if !compilation_succeeded {
            tracing::error!(
                "{} failed in {:.2?}",
                event.event_noun().first_char_to_ascii_uppercase(),
                compilation_start.elapsed()
            );
        } else {
            tracing::info!(
                "{} Finished {} in {:.2?}",
                "All good!".if_supports_color(Stdout, |text| text.green()),
                event.event_noun(),
                compilation_start.elapsed()
            );
        }

        error_log_result?;
        hook_result?;
        if compilation_succeeded {
            // Run the eval commands, if any.
            self.eval(log).await?;
            // Run the user-provided test command, if any.
            self.test(log).await?;
        }

        Ok(())
    }

    async fn run_shell_lifecycle_events<const N: usize>(
        &mut self,
        events: [LifecycleEvent; N],
    ) -> eyre::Result<()> {
        for event in events {
            self.opts
                .hooks
                .run_shell_hooks(event, &mut self.command_handles)
                .await?;
        }
        self.prune_command_handles();
        Ok(())
    }

    #[instrument(skip_all, fields(%event), level = "trace")]
    async fn run_hooks(
        &mut self,
        event: LifecycleEvent,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        // Before-hooks must not lose their shell commands merely because the GHCi prompt has
        // disappeared. Run shell commands first; if a subsequent GHCi hook encounters a broken
        // pipe, the external lifecycle notification has already happened.
        let shell_first = matches!(event, LifecycleEvent::Reload(hooks::When::Before));
        if shell_first {
            self.opts
                .hooks
                .run_shell_hooks(event, &mut self.command_handles)
                .await?;
        }

        for hook in self.opts.hooks.select(event) {
            if shell_first && matches!(&hook.command, hooks::Command::Shell(_)) {
                continue;
            }
            tracing::info!(command = %hook.command, "Running {hook} command");
            match &hook.command {
                hooks::Command::Ghci(command) => {
                    let start_time = Instant::now();
                    if matches!(hook.event, LifecycleEvent::Test) {
                        self.stdin
                            .run_command(&mut self.stdout, command, log)
                            .await?;
                        tracing::info!("Finished running tests in {:.2?}", start_time.elapsed());
                    } else {
                        // Hook diagnostics are advisory and must not turn the surrounding reload
                        // into a failed compilation or suppress its after-hooks.
                        let mut hook_log = CompilationLog::default();
                        self.stdin
                            .run_command(&mut self.stdout, command, &mut hook_log)
                            .await?;
                        if matches!(hook_log.result(), Some(CompilationResult::Err)) {
                            tracing::error!(%command, "Ignoring {hook} command error");
                        }
                    }
                }
                hooks::Command::Shell(command) => {
                    if let Err(err) = command.run_on(&mut self.command_handles).await {
                        tracing::error!(%command, "Ignoring {hook} command error: {err}");
                    }
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self), level = "trace")]
    async fn write_error_log(&mut self, log: &CompilationLog) -> eyre::Result<()> {
        self.error_log.write(log).await
    }
}

fn error_is_broken_pipe(err: &eyre::Report) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

/// How a [`Ghci`] session responds to a reload event.
#[derive(Debug, Clone, Copy)]
pub enum GhciReloadKind {
    /// Reload classification and before-hooks have not completed yet.
    Pending,
    /// No interruptible work remains. This includes post-compilation hooks.
    None,
    /// Reload, add, and/or remove modules. Can be interrupted.
    Reload,
    /// Restart the whole session. Cannot be interrupted.
    Restart,
}
