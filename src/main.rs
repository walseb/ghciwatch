//! `ghciwatch` is a `ghci`-based file watcher and recompiler for Haskell projects, leveraging
//! Haskell's interpreted mode for faster reloads.
//!
//! `ghciwatch` watches your modules for changes and reloads them in a `ghci` session, displaying
//! any errors.

use std::time::Duration;

use clap::CommandFactory;
use clap::Parser;
use eyre::eyre;
use ghciwatch::cli;
use ghciwatch::cli::ExperimentalFeature;
use ghciwatch::run_ghci;
use ghciwatch::run_tui;
use ghciwatch::run_watcher;
use ghciwatch::GhciOpts;
use ghciwatch::ShutdownManager;
use ghciwatch::TracingOpts;
use ghciwatch::WatcherCommand;
use ghciwatch::WatcherOpts;
use tokio::sync::mpsc;

const PARENT_CHECK_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Capture this before initialization can block or yield. If the launcher disappears while
    // options, tracing, or GHCi are starting, the monitor still recognizes the reparenting.
    let original_parent = nix::unistd::getppid();
    color_eyre::install()?;
    let mut opts = cli::Opts::parse();
    opts.init()?;

    if opts.tui {
        return Err(eyre!(
            "`--tui` has been removed. Please use `--experimental-features tui` instead."
        ));
    }

    let (maybe_tracing_reader, _tracing_guard) = TracingOpts::from_cli(&opts).install()?;

    if !opts.experimental_features.is_empty() {
        tracing::warn!(
            "`--experimental-features` may contain bugs or change drastically in future releases."
        );
    }

    #[cfg(feature = "clap-markdown")]
    if opts.generate_markdown_help {
        println!("{}", ghciwatch::clap_markdown::help_markdown::<cli::Opts>());
        return Ok(());
    }

    #[cfg(feature = "clap_mangen")]
    if let Some(out_dir) = opts.generate_man_pages {
        use eyre::WrapErr;

        let command = cli::Opts::command();
        clap_mangen::generate_to(command, out_dir).wrap_err("Failed to generate man pages")?;
        return Ok(());
    }

    if let Some(shell) = opts.completions {
        let mut command = cli::Opts::command();
        clap_complete::generate(shell, &mut command, "ghciwatch", &mut std::io::stdout());
        return Ok(());
    }

    std::env::set_var("IN_GHCIWATCH", "1");

    let (ghci_sender, ghci_receiver) = mpsc::channel(32);
    let (watcher_command_sender, watcher_command_receiver) = mpsc::channel::<WatcherCommand>(8);

    let (ghci_opts, maybe_ghci_reader) = GhciOpts::from_cli(&opts)?;
    let watcher_opts = WatcherOpts::from_cli(&opts)?;

    let mut manager =
        ShutdownManager::with_timeout(ghci_opts.shutdown_timeout(Duration::from_secs(1)));

    if opts.has_experimental_feature(ExperimentalFeature::Tui) {
        let tracing_reader =
            maybe_tracing_reader.expect("`tracing_reader` must be present if `tui` is given");
        let ghci_reader =
            maybe_ghci_reader.expect("`tui_reader` must be present if `tui` is given");
        manager
            .spawn("run_tui", |handle| {
                run_tui(handle, ghci_reader, tracing_reader)
            })
            .await;
    }

    manager
        .spawn("run_ghci", |handle| {
            run_ghci(handle, ghci_opts, ghci_receiver, watcher_command_sender)
        })
        .await;
    manager
        .spawn("run_watcher", move |handle| {
            run_watcher(handle, ghci_sender, watcher_command_receiver, watcher_opts)
        })
        .await;
    // Subscribe this last so every long-running task is already able to receive its shutdown
    // broadcast if the original parent disappeared during initialization.
    manager
        .spawn("watch_parent", move |handle| {
            watch_parent(handle, original_parent)
        })
        .await;
    let ret = manager.wait_for_shutdown().await;
    tracing::debug!("main() finished");
    ret
}

/// Request normal graceful shutdown once this process is no longer owned by its launch parent.
async fn watch_parent(
    mut handle: ghciwatch::ShutdownHandle,
    original_parent: nix::unistd::Pid,
) -> eyre::Result<()> {
    loop {
        let current_parent = nix::unistd::getppid();
        if current_parent != original_parent {
            tracing::info!(
                original_parent = original_parent.as_raw(),
                current_parent = current_parent.as_raw(),
                "Parent process exited; shutting down ghciwatch"
            );
            // The manager itself keeps a broadcast receiver alive. Ignore a send error anyway: it
            // means shutdown has already progressed far enough that no receiver remains.
            let _ = handle.request_shutdown();
            return Ok(());
        }

        tokio::select! {
            _ = tokio::time::sleep(PARENT_CHECK_INTERVAL) => {}
            _ = handle.on_shutdown_requested() => return Ok(()),
        }
    }
}
