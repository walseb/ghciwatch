//! Failure-gating shell commands, separate from advisory lifecycle hooks.
use std::process::Stdio;

use eyre::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::{CompilationLog, ErrorLog, GhciOpts, GhciWriter};
use crate::clonable_command::ClonableCommand;
use crate::normal_path::NormalPath;

pub(super) async fn run(opts: &GhciOpts) -> eyre::Result<()> {
    if opts.setup_shell.is_empty() {
        return Ok(());
    }
    let mut updates = opts.setup_updates.clone();
    let mut error_log = ErrorLog::new(
        opts.error_path
            .as_ref()
            .map(NormalPath::from_cwd)
            .transpose()?,
    );
    loop {
        // Ignore notifications preceding this attempt, but retain edits made while it runs.
        updates.borrow_and_update();
        let mut failure = None;
        for command in &opts.setup_shell {
            tracing::info!(%command, "Running setup-shell");
            let started = std::time::Instant::now();
            let result = execute(command, opts).await;
            tracing::info!(%command, elapsed = ?started.elapsed(), "Finished setup-shell");
            match result {
                Ok(None) => {}
                Ok(Some(message)) => {
                    failure = Some(message);
                    break;
                }
                Err(error) => {
                    failure = Some(format!(
                        "setup-shell command `{command}` failed: {error:#}\n"
                    ));
                    break;
                }
            }
        }
        let Some(message) = failure else {
            return Ok(());
        };
        let mut log = CompilationLog::default();
        log.mark_failed_with_diagnostic(&message);
        error_log.write(&log).await?;
        tracing::error!(
            "{message}Waiting for a watched file to change before retrying setup-shell"
        );
        updates.changed().await.wrap_err("Setup watcher closed")?;
    }
}

async fn execute(command: &ClonableCommand, opts: &GhciOpts) -> eyre::Result<Option<String>> {
    let mut child = command
        .as_tokio()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, stdout, stderr) = tokio::try_join!(
        child.wait(),
        capture(stdout, opts.stdout_writer.clone()),
        capture(stderr, opts.stderr_writer.clone()),
    )?;
    Ok((!status.success()).then(|| {
        format!(
            "setup-shell command `{command}` failed ({status})\n{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr),
        )
    }))
}

/// Drain both pipes concurrently and forward immediately, while retaining failure diagnostics.
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    mut writer: GhciWriter,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&buffer[..count]);
        writer.write_all(&buffer[..count]).await?;
        writer.flush().await?;
    }
}
