use std::fmt::Display;
use std::fmt::Write;
use std::process::ExitStatus;
use std::process::Stdio;
use std::str::FromStr;

use eyre::eyre;
use eyre::Context;
use tokio::task::JoinHandle;
use tracing::instrument;
use tracing::Instrument;
use winnow::combinator::opt;
use winnow::combinator::rest;
use winnow::PResult;
use winnow::Parser;

use crate::clonable_command::ClonableCommand;
use crate::command_ext::CommandExt;

/// A shell command which may optionally be run asynchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaybeAsyncCommand {
    /// Should this command be run asynchronously?
    pub is_async: bool,
    /// The contained command.
    pub command: ClonableCommand,
}

impl Display for MaybeAsyncCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.command.fmt(f)
    }
}

impl FromStr for MaybeAsyncCommand {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_maybe_async_command
            .parse(s)
            .map_err(|err| eyre!("{err}"))
    }
}

fn parse_maybe_async_command(input: &mut &str) -> PResult<MaybeAsyncCommand> {
    let is_async = opt("async:").parse_next(input)?.is_some();

    let command = rest.parse_to().parse_next(input)?;

    Ok(MaybeAsyncCommand { is_async, command })
}

/// Run lifecycle commands as children owned by ghciwatch. Hook output is intentionally inherited so
/// it remains visible while the hook runs instead of being buffered in memory. Synchronous hooks
/// are awaited; `async:` hooks remain tracked in the background. If either runner is cancelled,
/// `kill_on_drop` prevents its child from outliving ghciwatch.
impl MaybeAsyncCommand {
    #[instrument(skip(self), fields(%self), level = "debug")]
    pub async fn status(&self) -> MaybeAsyncCommandStatus {
        let program = self.command.program.to_string_lossy().into_owned();
        let mut command = self.command.as_tokio();
        command.kill_on_drop(true);
        let command_formatted = self.display();
        let join_handle = tokio::task::spawn(
            async move {
                tracing::info!("$ {command_formatted}");
                // Let hook output stream directly rather than retaining the complete output in
                // memory until the command exits.
                let status = command
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .status()
                    .await
                    .wrap_err_with(|| format!("Failed to execute `{command_formatted}`"))?;

                let mut message = shell_words::quote(&program).into_owned();
                message.push(' ');
                if status.success() {
                    message.push_str("finished successfully");
                    tracing::debug!("{message}");
                } else {
                    write!(message, "failed: {status}").expect("Writing to a `String` never fails");
                    tracing::error!("{message}");
                }

                Ok(status)
            }
            .instrument(tracing::debug_span!("status").or_current()),
        );

        if self.is_async {
            MaybeAsyncCommandStatus::Async(join_handle)
        } else {
            let command_formatted = self.display();
            let status = join_handle
                .await
                .wrap_err_with(|| format!("Panicked while executing `{command_formatted}`"))
                .and_then(std::convert::identity);
            MaybeAsyncCommandStatus::Sync(status)
        }
    }

    /// Run this command.
    ///
    /// If it's a synchronous command, report its status. Otherwise, add the [`JoinHandle`] for its
    /// task to the given list of handles.
    pub async fn run_on(
        &self,
        handles: &mut Vec<JoinHandle<eyre::Result<ExitStatus>>>,
    ) -> eyre::Result<()> {
        match self.status().await {
            MaybeAsyncCommandStatus::Sync(result) => {
                // If we failed to execute the program, that's an actual error, but if the
                // program failed on its own, we'll log and move on.
                result?;
            }
            MaybeAsyncCommandStatus::Async(join_handle) => {
                // If the program is running asynchronously, we'll store the `JoinHandle`
                // so we don't kill it and so we can log when it completes.
                handles.push(join_handle);
            }
        }
        Ok(())
    }
}

pub enum MaybeAsyncCommandStatus {
    Sync(eyre::Result<ExitStatus>),
    Async(JoinHandle<eyre::Result<ExitStatus>>),
}

impl CommandExt for MaybeAsyncCommand {
    fn display(&self) -> String {
        self.command.display()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        assert_eq!(
            "puppy --flavor 'sammy' --eyes \"brown\""
                .parse::<MaybeAsyncCommand>()
                .unwrap(),
            MaybeAsyncCommand {
                is_async: false,
                command: ClonableCommand::new("puppy")
                    .args(["--flavor", "sammy", "--eyes", "brown"])
            }
        );

        assert_eq!(
            "async: puppy --flavor 'sammy' --eyes \"brown\""
                .parse::<MaybeAsyncCommand>()
                .unwrap(),
            MaybeAsyncCommand {
                is_async: true,
                command: ClonableCommand::new("puppy")
                    .args(["--flavor", "sammy", "--eyes", "brown"])
            }
        );
    }
}
