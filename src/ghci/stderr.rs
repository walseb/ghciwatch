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

    /// Read through an exact marker line and return the preceding buffered output.
    /// Once the marker written by GHCi has been consumed, all earlier stderr is in the buffer.
    GetBufferThrough {
        marker: String,
        ready: oneshot::Sender<()>,
        sender: oneshot::Sender<String>,
    },
}

pub struct GhciStderr {
    pub shutdown: ShutdownHandle,
    pub reader: Lines<BufReader<ChildStderr>>,
    pub writer: GhciWriter,
    pub receiver: mpsc::Receiver<StderrEvent>,
    /// Output buffer.
    pub buffer: String,
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
            StderrEvent::GetBufferThrough {
                marker,
                ready,
                sender,
            } => {
                let _ = ready.send(());
                self.get_buffer_through(&marker, sender).await?;
            }
        }

        Ok(())
    }

    #[instrument(skip(self), level = "trace")]
    async fn ingest_line(&mut self, mut line: String) -> eyre::Result<()> {
        tracing::debug!(line, "Read stderr line");
        line.push('\n');
        self.buffer.push_str(&line);
        self.writer.write_all(line.as_bytes()).await?;
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
            if line == marker {
                break;
            }
            self.ingest_line(line).await?;
        }
        let _ = sender.send(self.buffer.clone());
        Ok(())
    }
}
