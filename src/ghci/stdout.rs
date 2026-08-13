use std::time::Duration;

use aho_corasick::AhoCorasick;
use eyre::Context;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::instrument;

use crate::aho_corasick::AhoCorasickExt;
use crate::incremental_reader::FindAt;
use crate::incremental_reader::IncrementalReader;
use crate::incremental_reader::ReadUntilStatus;
use crate::incremental_reader::ReadOpts;
use crate::incremental_reader::WriteBehavior;

use super::parse::parse_ghc_messages;
use super::parse::parse_show_paths;
use super::parse::parse_show_targets;
use super::parse::ShowPaths;
use super::stderr::StderrEvent;
use super::writer::GhciWriter;
use super::CompilationLog;
use super::ModuleSet;

pub struct GhciStdout {
    /// Reader for parsing and forwarding the underlying stdout stream.
    pub reader: IncrementalReader<ChildStdout, GhciWriter>,
    /// Channel for communicating with the stderr task.
    pub stderr_sender: mpsc::Sender<StderrEvent>,
    /// Prompt patterns to match. Constructing these `AhoCorasick` automatons is costly so we store
    /// them in the task state.
    pub prompt_patterns: AhoCorasick,
    /// A buffer to read data into. Lets us avoid allocating buffers in the [`IncrementalReader`].
    pub buffer: Vec<u8>,
    /// Nonce used to make stderr synchronization markers unique within this session.
    pub stderr_sync_nonce: u64,
}

impl GhciStdout {
    #[instrument(skip_all, level = "debug")]
    async fn parse_into_log(
        &mut self,
        stdin: &mut ChildStdin,
        data: &str,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        // Stdout and stderr are independent pipes. A stdout prompt does not prove that the stderr
        // task has consumed all diagnostics emitted by the command. Submit a subsequent marker
        // command and read stderr through it before parsing the operation's output.
        let stderr_data = self.stderr_buffer_through_marker(stdin).await?;
        log.extend(parse_ghc_messages(data).wrap_err("Failed to parse compiler output")?);
        log.extend(parse_ghc_messages(&stderr_data).wrap_err("Failed to parse compiler output")?);
        Ok(())
    }

    async fn stderr_buffer_through_marker(
        &mut self,
        stdin: &mut ChildStdin,
    ) -> eyre::Result<String> {
        let marker = format!(
            "__GHCIWATCH_STDERR_END_{}_{}__",
            std::process::id(),
            self.stderr_sync_nonce
        );
        self.stderr_sync_nonce = self.stderr_sync_nonce.wrapping_add(1);
        let (ready_sender, ready_receiver) = oneshot::channel();
        let (sender, receiver) = oneshot::channel();
        self.stderr_sender
            .send(StderrEvent::GetBufferThrough {
                marker: marker.clone(),
                ready: ready_sender,
                sender,
            })
            .await?;
        ready_receiver.await?;

        stdin
            .write_all(format!(":! printf '%s\\n' '{marker}' >&2\n").as_bytes())
            .await?;
        // Consume the marker command's stdout prompt. The shell command emits no stdout, so GHCi
        // may print this prompt directly after the previously consumed prompt, without a newline.
        let _ = self
            .reader
            .read_until(&mut ReadOpts {
                end_marker: &self.prompt_patterns,
                find: FindAt::Anywhere,
                writing: WriteBehavior::NoFinalLine,
                buffer: &mut self.buffer,
            })
            .await?;
        Ok(receiver.await?)
    }

    #[instrument(skip_all, name = "stdout_initialize", level = "debug")]
    pub async fn initialize(&mut self, log: &mut CompilationLog) -> eyre::Result<()> {
        // Wait for `ghci` to start up. This may involve compiling a bunch of stuff.
        let bootup_patterns = AhoCorasick::from_anchored_patterns([
            "GHCi, version ",
            "GHCJSi, version ",
            "Clashi, version ",
        ]);
        let data = self
            .reader
            .read_until(&mut ReadOpts {
                end_marker: &bootup_patterns,
                find: FindAt::LineStart,
                writing: WriteBehavior::Write,
                buffer: &mut self.buffer,
            })
            .await?;
        tracing::debug!(data, "ghci started, saw version marker");

        // The configured prompt is not active yet, so initialization cannot use a marker command.
        // Parse startup stdout now, but leave stderr buffered. `GhciStdin::initialize` installs the
        // prompt without clearing stderr and then uses the normal marker boundary to collect every
        // startup diagnostic, including output delayed beyond the version banner.
        log.extend(parse_ghc_messages(&data).wrap_err("Failed to parse compiler output")?);

        Ok(())
    }

    /// Clear stderr diagnostics from the preceding GHCi operation.
    ///
    /// This must complete before the next command is written. Otherwise a fast
    /// diagnostic can reach the stderr task before a delayed clear and be lost.
    pub async fn clear_stderr_buffer(&self) -> eyre::Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.stderr_sender
            .send(StderrEvent::ClearBuffer { sender })
            .await?;
        receiver.await?;
        Ok(())
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn prompt(
        &mut self,
        stdin: &mut ChildStdin,
        find: FindAt,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        let data = self
            .reader
            .read_until(&mut ReadOpts {
                end_marker: &self.prompt_patterns,
                find,
                writing: WriteBehavior::NoFinalLine,
                buffer: &mut self.buffer,
            })
            .await?;
        tracing::debug!(bytes = data.len(), "Got data from ghci");

        self.parse_into_log(stdin, &data, log).await?;
        Ok(())
    }

    /// Wait for the GHCi prompt, but stop if no `Compiling` progress is seen for the timeout.
    /// Unrelated output is still forwarded but does not keep a wedged compilation alive.
    #[instrument(skip_all, level = "debug")]
    pub async fn prompt_with_progress_timeout(
        &mut self,
        stdin: &mut ChildStdin,
        find: FindAt,
        log: &mut CompilationLog,
        progress_timeout: Duration,
    ) -> eyre::Result<bool> {
        let result = self
            .reader
            .read_until_with_progress_timeout(
                &mut ReadOpts {
                    end_marker: &self.prompt_patterns,
                    find,
                    writing: WriteBehavior::NoFinalLine,
                    buffer: &mut self.buffer,
                },
                progress_timeout,
                "Compiling",
            )
            .await?;
        match result {
            ReadUntilStatus::Complete(data) => {
                tracing::debug!(bytes = data.len(), "Got data from ghci");
                self.parse_into_log(stdin, &data, log).await?;
                Ok(true)
            }
            ReadUntilStatus::Inactive => Ok(false),
        }
    }

    /// Read any immediately-available output from the pipe, then drain stale prompts from
    /// the internal buffer. Returns the number of prompts found and discarded.
    pub async fn buffer_and_drain_prompts(&mut self, timeout: Duration) -> eyre::Result<usize> {
        self.reader
            .buffer_available(&mut self.buffer, timeout, WriteBehavior::NoFinalLine)
            .await?;

        self.reader
            .drain_buffered_chunks(&ReadOpts {
                end_marker: &self.prompt_patterns,
                find: FindAt::Anywhere,
                writing: WriteBehavior::NoFinalLine,
                buffer: &mut self.buffer,
            })
            .await
    }

    /// Read stdout until the given marker string is found, discarding everything before it.
    ///
    /// Used by `send_sigint` to synchronize with GHCi after an interrupt: a sync expression
    /// is sent on stdin and this method reads until its output appears, guaranteeing that all
    /// prior output has been consumed.
    pub async fn read_until_marker(&mut self, marker: &str) -> eyre::Result<String> {
        let pattern = AhoCorasick::from_anchored_patterns([marker]);
        self.reader
            .read_until(&mut ReadOpts {
                end_marker: &pattern,
                find: FindAt::Anywhere,
                writing: WriteBehavior::NoFinalLine,
                buffer: &mut self.buffer,
            })
            .await
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn show_paths(&mut self) -> eyre::Result<ShowPaths> {
        let lines = self
            .reader
            .read_until(&mut ReadOpts {
                end_marker: &self.prompt_patterns,
                find: FindAt::LineStart,
                writing: WriteBehavior::Hide,
                buffer: &mut self.buffer,
            })
            .await?;
        parse_show_paths(&lines).wrap_err("Failed to parse `:show paths` output")
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn show_targets(&mut self, search_paths: &ShowPaths) -> eyre::Result<ModuleSet> {
        let lines = self
            .reader
            .read_until(&mut ReadOpts {
                end_marker: &self.prompt_patterns,
                find: FindAt::LineStart,
                writing: WriteBehavior::Hide,
                buffer: &mut self.buffer,
            })
            .await?;
        parse_show_targets(search_paths, &lines).wrap_err("Failed to parse `:show targets` output")
    }

    #[allow(dead_code)] // TODO: No it should not be!
    #[instrument(skip_all, level = "debug")]
    pub async fn quit(&mut self) -> eyre::Result<()> {
        let leaving_ghci = AhoCorasick::from_anchored_patterns(["Leaving GHCi."]);
        let data = self
            .reader
            .read_until(&mut ReadOpts {
                end_marker: &leaving_ghci,
                find: FindAt::Anywhere,
                writing: WriteBehavior::Write,
                buffer: &mut self.buffer,
            })
            .await?;
        tracing::debug!(data, "ghci confirmed quit on stdout");
        Ok(())
    }
}
