use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;
use eyre::Context;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::Lines;
use tokio::process::ChildStderr;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::instrument;

use crate::shutdown::ShutdownHandle;

use super::writer::GhciWriter;

/// An event sent to a `ghci` session's stderr channel.
#[derive(Debug)]
pub enum StderrEvent {
    /// Clear the buffer contents and acknowledge when it has been cleared.
    ClearBuffer { sender: oneshot::Sender<()> },

    /// Enable or disable forwarding while continuing to capture stderr for parsing.
    SetForwarding {
        enabled: bool,
        /// When re-enabling, publish all output captured while forwarding was disabled.
        replay_buffer: bool,
        sender: oneshot::Sender<()>,
    },

    /// Arm one compilation operation to be notified when GHC emits an error diagnostic.
    InterruptOnError { sender: oneshot::Sender<()> },

    /// Stop notifying the preceding compilation operation.
    DisarmInterruptOnError,

    /// Read through an exact marker line and return the preceding buffered output.
    /// Once the marker written by GHCi has been consumed, all earlier stderr is in the buffer.
    GetBufferThrough {
        marker: String,
        ready: oneshot::Sender<()>,
        sender: oneshot::Sender<String>,
    },

    /// The GHCi command exited before a synchronization marker could be submitted. Drain stderr
    /// through EOF and return all buffered startup output.
    DrainBuffer { sender: oneshot::Sender<String> },
}

pub struct GhciStderr {
    pub shutdown: ShutdownHandle,
    pub reader: Lines<BufReader<ChildStderr>>,
    pub writer: GhciWriter,
    pub receiver: mpsc::Receiver<StderrEvent>,
    /// Output buffer.
    pub buffer: String,
    /// Whether newly ingested lines are forwarded to the configured stderr writer.
    pub forwarding: bool,
    /// Lines captured while forwarding is disabled, retained across per-command buffer clears.
    pub suppressed_buffer: String,
    /// Notification for the currently active interrupt-on-error compilation operation.
    pub interrupt_on_error: Option<oneshot::Sender<()>>,
}

impl GhciStderr {
    #[instrument(skip_all, name = "stderr", level = "debug")]
    pub async fn run(mut self) -> eyre::Result<()> {
        let mut backoff = ExponentialBackoff::default();
        while let Some(duration) = backoff.next_backoff() {
            match self.run_inner().await {
                Ok(()) => {
                    // MPSC channel closed, probably a graceful shutdown?
                    break;
                }
                Err(err) => {
                    tracing::error!("{err:?}");
                }
            }

            tracing::debug!("Waiting {duration:?} before retrying");
            tokio::time::sleep(duration).await;
        }

        Ok(())
    }

    pub async fn run_inner(&mut self) -> eyre::Result<()> {
        loop {
            tokio::select! {
                Ok(Some(line)) = self.reader.next_line() => {
                    self.ingest_line(line).await?;
                }
                Some(event) = self.receiver.recv() => {
                    self.dispatch(event).await?;
                }
                _ = self.shutdown.on_shutdown_requested() => {
                    // Graceful exit.
                    break;
                }
                else => {
                    // Graceful exit.
                    break;
                }
            }
        }
        Ok(())
    }

    async fn dispatch(&mut self, event: StderrEvent) -> eyre::Result<()> {
        match event {
            StderrEvent::ClearBuffer { sender } => {
                self.clear_buffer().await;
                let _ = sender.send(());
            }
            StderrEvent::SetForwarding {
                enabled,
                replay_buffer,
                sender,
            } => {
                if enabled && !self.forwarding {
                    if replay_buffer {
                        self.writer
                            .write_all(self.suppressed_buffer.as_bytes())
                            .await?;
                        self.writer.flush().await?;
                    }
                    self.suppressed_buffer.clear();
                } else if !enabled && self.forwarding {
                    self.suppressed_buffer.clear();
                }
                self.forwarding = enabled;
                let _ = sender.send(());
            }
            StderrEvent::InterruptOnError { sender } => {
                self.interrupt_on_error = Some(sender);
            }
            StderrEvent::DisarmInterruptOnError => {
                self.interrupt_on_error = None;
            }
            StderrEvent::GetBufferThrough {
                marker,
                ready,
                sender,
            } => {
                let _ = ready.send(());
                self.get_buffer_through(&marker, sender).await?;
            }
            StderrEvent::DrainBuffer { sender } => {
                self.drain_buffer(sender).await?;
            }
        }

        Ok(())
    }

    #[instrument(skip(self), level = "trace")]
    async fn ingest_line(&mut self, mut line: String) -> eyre::Result<()> {
        if self.forwarding {
            tracing::debug!(line, "Read stderr line");
        } else {
            tracing::debug!(line, "Read suppressed stderr line");
        }
        line.push('\n');
        if line_has_error_diagnostic(&line) {
            if let Some(sender) = self.interrupt_on_error.take() {
                let _ = sender.send(());
            }
        }
        self.buffer.push_str(&line);
        if !self.forwarding {
            self.suppressed_buffer.push_str(&line);
        }
        if self.forwarding {
            self.writer.write_all(line.as_bytes()).await?;
            // Do not rely on terminal line buffering: output may be redirected to a block-buffered
            // destination, and callers expect diagnostics to become visible one line at a time.
            self.writer.flush().await?;
        }
        Ok(())
    }

    #[instrument(skip(self), level = "trace")]
    async fn clear_buffer(&mut self) {
        self.buffer.clear();
    }

    #[instrument(skip(self, sender), level = "debug")]
    async fn get_buffer_through(
        &mut self,
        marker: &str,
        mut sender: oneshot::Sender<String>,
    ) -> eyre::Result<()> {
        loop {
            // A caller can be cancelled after registering a marker but before writing it. Do not
            // strand the stderr task: that would block every later synchronization request.
            let line = tokio::select! {
                _ = sender.closed() => return Ok(()),
                line = self.reader.next_line() => line,
            }
            .wrap_err("Failed to read stderr while waiting for synchronization marker")?
            .ok_or_else(|| {
                eyre::eyre!("GHCi stderr closed while waiting for synchronization marker")
            })?;
            if let Some(diagnostic_line) = line_without_marker(&line, marker) {
                // GHC's orphaned parallel logger can write concurrently with the marker shell
                // command, splicing the marker into a diagnostic line. Retain its surrounding text.
                if !diagnostic_line.is_empty() {
                    self.ingest_line(diagnostic_line).await?;
                }
                break;
            }
            self.ingest_line(line).await?;
        }
        let _ = sender.send(self.buffer.clone());
        Ok(())
    }

    /// Drain the command's stderr after its stdout/stdin pipes have already closed.
    #[instrument(skip(self, sender), level = "debug")]
    async fn drain_buffer(&mut self, sender: oneshot::Sender<String>) -> eyre::Result<()> {
        while let Some(line) = self
            .reader
            .next_line()
            .await
            .wrap_err("Failed to drain stderr after GHCi exited")?
        {
            self.ingest_line(line).await?;
        }
        let _ = sender.send(self.buffer.clone());
        Ok(())
    }
}

fn line_without_marker(line: &str, marker: &str) -> Option<String> {
    line.contains(marker).then(|| line.replacen(marker, "", 1))
}

fn line_has_error_diagnostic(line: &str) -> bool {
    use crate::ghci::parse::GhcMessage;
    use crate::ghci::parse::Severity;

    crate::ghci::parse::parse_ghc_messages(line).is_ok_and(|messages| {
        messages.into_iter().any(|message| {
            matches!(
                message,
                GhcMessage::Diagnostic(diagnostic) if diagnostic.severity == Severity::Error
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::line_has_error_diagnostic;
    use super::line_without_marker;

    #[test]
    fn recognizes_only_ghc_error_headers() {
        assert!(line_has_error_diagnostic(
            "src/Foo.hs:4:11: error: [GHC-12345]\n"
        ));
        assert!(line_has_error_diagnostic("<no location info>: error:\n"));
        assert!(!line_has_error_diagnostic("src/Foo.hs:4:11: warning:\n"));
        assert!(!line_has_error_diagnostic(
            "application said error: but this is not a diagnostic\n"
        ));
    }

    #[test]
    fn recognizes_marker_spliced_into_diagnostic() {
        let marker = "__GHCIWATCH_STDERR_END_872490_463__";
        assert_eq!(
            line_without_marker(
                &format!("558 | value = Zspirv.Signed   Zspirv.W32{marker}^^^^^^^"),
                marker,
            ),
            Some("558 | value = Zspirv.Signed   Zspirv.W32^^^^^^^".to_owned())
        );
        assert_eq!(
            line_without_marker(&format!("{marker}diagnostic"), marker),
            Some("diagnostic".to_owned())
        );
        assert_eq!(
            line_without_marker(&format!("diagnostic{marker}"), marker),
            Some("diagnostic".to_owned())
        );
        assert_eq!(line_without_marker(marker, marker), Some(String::new()));
        assert_eq!(line_without_marker("diagnostic", marker), None);
    }
}
