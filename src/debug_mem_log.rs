//! Process-wide diagnostic logging, including output inherited by shell hooks.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;

use eyre::Context;

pub fn install(path: &Path) -> eyre::Result<()> {
    eyre::ensure!(
        cfg!(target_os = "linux"),
        "--debug-mem requires Linux /proc"
    );
    let file = std::fs::File::create(path)
        .wrap_err_with(|| format!("Cannot create debug memory log {}", path.display()))?;
    eprintln!("Debug memory run log: {}", path.display());
    std::io::stdout().flush()?;
    std::io::stderr().flush()?;
    // Install before tracing or subprocess creation. Both descriptors share the same
    // open-file offset, so inherited hook output cannot overwrite compiler output.
    nix::unistd::dup2(file.as_raw_fd(), 1)?;
    nix::unistd::dup2(file.as_raw_fd(), 2)?;
    Ok(())
}
