use std::time::Duration;

use eyre::Context;
use itertools::Itertools;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tracing::instrument;

use crate::incremental_reader::FindAt;

use super::loaded_module::LoadedModule;
use super::parse::ShowPaths;
use super::CompilationLog;
use super::GhciCommand;
use super::ModuleSet;
use super::PROMPT;
use crate::ghci::GhciStdout;

pub struct GhciStdin {
    /// Inner stdin writer.
    pub stdin: ChildStdin,
}

impl GhciStdin {
    /// Write a line on `stdin` and wait for a prompt on stdout.
    ///
    /// The `line` should contain the trailing newline.
    ///
    /// The `find` parameter determines where the prompt can be found in the output line.
    #[instrument(skip(self, stdout), level = "debug")]
    async fn write_line_with_prompt_at(
        &mut self,
        stdout: &mut GhciStdout,
        line: &str,
        find: FindAt,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        stdout.clear_stderr_buffer().await?;
        self.stdin.write_all(line.as_bytes()).await?;
        stdout.prompt(&mut self.stdin, find, log).await
    }

    /// Write a line on `stdin` and wait for a prompt on stdout.
    ///
    /// The `line` should contain the trailing newline.
    async fn write_line(
        &mut self,
        stdout: &mut GhciStdout,
        line: &str,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        self.write_line_with_prompt_at(stdout, line, FindAt::LineStart, log)
            .await
    }

    /// Write a line and wait until either the prompt arrives or compilation progress stops for the
    /// supplied timeout. Returns `false` when no `Compiling` line appears before the deadline.
    async fn write_line_with_progress_timeout(
        &mut self,
        stdout: &mut GhciStdout,
        line: &str,
        log: &mut CompilationLog,
        progress_timeout: Duration,
    ) -> eyre::Result<bool> {
        stdout.clear_stderr_buffer().await?;
        self.stdin.write_all(line.as_bytes()).await?;
        stdout
            .prompt_with_progress_timeout(
                &mut self.stdin,
                FindAt::LineStart,
                log,
                progress_timeout,
            )
            .await
    }

    /// Run a [`GhciCommand`].
    ///
    /// The command may be multiple lines.
    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn run_command(
        &mut self,
        stdout: &mut GhciStdout,
        command: &GhciCommand,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        for line in command.lines() {
            self.write_line(stdout, &format!("{line}\n"), log).await?;
        }

        Ok(())
    }

    /// Write `:set prompt "{prompt}"\n` to stdin without reading any response.
    ///
    /// Callers that need to wait for GHCi to acknowledge the new prompt should use
    /// [`Self::set_prompt`] instead.
    pub async fn write_set_prompt(&mut self, prompt: &str) -> eyre::Result<()> {
        self.stdin
            .write_all(format!(":set prompt \"{prompt}\"\n").as_bytes())
            .await?;
        Ok(())
    }

    /// Set the GHCi prompt to the given string.
    ///
    /// This writes `:set prompt` and waits for GHCi to show the new prompt.
    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn set_prompt(
        &mut self,
        stdout: &mut GhciStdout,
        prompt: &str,
        find: FindAt,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        stdout.clear_stderr_buffer().await?;
        self.write_set_prompt(prompt).await?;
        stdout.prompt(&mut self.stdin, find, log).await
    }

    #[instrument(skip(self, stdout), name = "stdin_initialize", level = "debug")]
    pub async fn initialize(
        &mut self,
        stdout: &mut GhciStdout,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        // Startup compilation continues after the version banner. Do not clear stderr here: the
        // marker synchronized by `prompt` captures all startup diagnostics through this command.
        self.write_set_prompt(PROMPT).await?;
        stdout
            .prompt(&mut self.stdin, FindAt::Anywhere, log)
            .await?;
        self.write_line(stdout, &format!(":set prompt-cont {PROMPT}\n"), log)
            .await?;
        Ok(())
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn reload(
        &mut self,
        stdout: &mut GhciStdout,
        log: &mut CompilationLog,
        inactivity_timeout: Duration,
    ) -> eyre::Result<bool> {
        self.write_line_with_progress_timeout(
            stdout,
            ":reload\n",
            log,
            inactivity_timeout,
        )
        .await
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn add_modules(
        &mut self,
        stdout: &mut GhciStdout,
        modules: impl IntoIterator<Item = &LoadedModule>,
        log: &mut CompilationLog,
        inactivity_timeout: Duration,
    ) -> eyre::Result<bool> {
        let modules = modules.into_iter().format(" ");
        // We use `:add` because `:load` unloads all previously loaded modules:
        //
        // > All previously loaded modules, except package modules, are forgotten. The new set of
        // > modules is known as the target set. Note that :load can be used without any arguments
        // > to unload all the currently loaded modules and bindings.
        //
        // https://downloads.haskell.org/ghc/latest/docs/users_guide/ghci.html#ghci-cmd-:load
        self.write_line_with_progress_timeout(
            stdout,
            &format!(":add {modules}\n"),
            log,
            inactivity_timeout,
        )
        .await
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn remove_modules(
        &mut self,
        stdout: &mut GhciStdout,
        modules: impl IntoIterator<Item = &LoadedModule>,
        log: &mut CompilationLog,
        inactivity_timeout: Duration,
    ) -> eyre::Result<bool> {
        let modules = modules.into_iter().format(" ");
        self.write_line_with_progress_timeout(
            stdout,
            &format!(":unadd {modules}\n"),
            log,
            inactivity_timeout,
        )
        .await
    }

    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn interpret_module(
        &mut self,
        stdout: &mut GhciStdout,
        module: &LoadedModule,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        // `:add *` forces the module to be interpreted, even if it was already loaded from
        // bytecode. This is necessary to access the module's top-level binds for the eval feature.
        self.write_line(stdout, &format!(":add *{module}\n"), log)
            .await
    }

    /// Add a module's top level identifiers to scope with `:module + *{module_name}`.
    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn add_module_to_scope(
        &mut self,
        stdout: &mut GhciStdout,
        module_name: &str,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        self.write_line(stdout, &format!(":module + *{module_name}\n"), log)
            .await
    }

    /// Remove a module's top level identifiers to scope with `:module - *{module_name}`.
    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn remove_module_from_scope(
        &mut self,
        stdout: &mut GhciStdout,
        module_name: &str,
        log: &mut CompilationLog,
    ) -> eyre::Result<()> {
        self.write_line(stdout, &format!(":module - *{module_name}\n"), log)
            .await
    }

    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn show_paths(&mut self, stdout: &mut GhciStdout) -> eyre::Result<ShowPaths> {
        self.stdin.write_all(b":show paths\n").await?;

        stdout.show_paths().await
    }

    #[instrument(skip_all, level = "debug")]
    pub async fn show_targets(
        &mut self,
        stdout: &mut GhciStdout,
        show_paths: &ShowPaths,
    ) -> eyre::Result<ModuleSet> {
        self.stdin.write_all(b":show targets\n").await?;

        stdout.show_targets(show_paths).await
    }

    #[allow(dead_code)] // TODO: No it should not be!
    #[instrument(skip(self, stdout), level = "debug")]
    pub async fn quit(&mut self, stdout: &mut GhciStdout) -> eyre::Result<()> {
        self.stdin
            .write_all(b":quit\n")
            .await
            .wrap_err("Failed to tell ghci to `:quit`")?;
        stdout
            .quit()
            .await
            .wrap_err("Failed to wait for ghci to quit")
    }
}
