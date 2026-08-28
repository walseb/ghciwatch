use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::io::BufWriter;
use tracing::instrument;

use crate::normal_path::NormalPath;

use super::parse::CompilationResult;
use super::parse::ModulesLoaded;
use super::CompilationLog;

/// Error log writer.
///
/// This produces `ghcid`-compatible output, which can be consumed by `ghcid` plugins in your
/// editor of choice.
pub struct ErrorLog {
    path: Option<NormalPath>,
    temp_nonce: u64,
}

impl ErrorLog {
    /// Construct a new error log writer for the given path.
    pub fn new(path: Option<NormalPath>) -> Self {
        Self {
            path,
            temp_nonce: 0,
        }
    }

    /// Get the path this error log is written to.
    ///
    /// Paths in GHC error messages are written to this path.
    pub fn path(&self) -> Option<&NormalPath> {
        self.path.as_ref()
    }

    /// Write the error log, if any, with the given compilation summary and diagnostic messages.
    #[instrument(skip(self, log), name = "error_log_write", level = "debug")]
    pub async fn write(&mut self, log: &CompilationLog) -> eyre::Result<()> {
        let path = match &self.path {
            Some(path) => path,
            None => {
                tracing::debug!("No error log path, not writing");
                return Ok(());
            }
        };

        self.temp_nonce = self.temp_nonce.wrapping_add(1);
        let file_name = path.absolute().file_name().unwrap_or("compile");
        let temporary = path.absolute().with_file_name(format!(
            ".{file_name}.ghciwatch-{}-{}.tmp",
            std::process::id(),
            self.temp_nonce,
        ));
        let file = File::create(&temporary).await?;
        let mut writer = BufWriter::new(file);

        if let Some(summary) = log.summary {
            // `ghcid` only writes the headline if there's no errors.
            if let CompilationResult::Ok = summary.result {
                tracing::debug!(%path, "Writing 'All good'");
                let modules_loaded = if summary.modules_loaded != ModulesLoaded::Count(1) {
                    format!("{} modules", summary.modules_loaded)
                } else {
                    format!("{} module", summary.modules_loaded)
                };
                writer
                    .write_all(format!("All good ({modules_loaded})\n").as_bytes())
                    .await?;
            }
        }

        for diagnostic in &log.diagnostics {
            tracing::debug!(%diagnostic, "Writing diagnostic");
            writer.write_all(diagnostic.to_string().as_bytes()).await?;
        }

        // This is load-bearing! If we don't properly flush/shutdown the handle, nothing gets
        // written!
        writer.shutdown().await?;
        tokio::fs::rename(&temporary, path.absolute()).await?;

        Ok(())
    }
}
