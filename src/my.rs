use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eyre::Context;
use nix::fcntl::{flock, FlockArg};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex, Notify};

use crate::ghci::{Ghci, StderrEvent};
use crate::incremental_reader::{FindAt, ReadOpts, WriteBehavior};

const MAX_COMMAND_BYTES: u64 = 1024 * 1024;
const COMMAND_TERMINATOR: &[u8] = "⋳".as_bytes();

/// Prevents socket commands from entering GHCi while a reload is pending or active.
///
/// The flag is set before the reload task starts and remains set while an interrupted
/// reload is cleaned up. Socket commands recheck it after acquiring the GHCi mutex,
/// closing the race between observing an idle state and entering GHCi.
pub(crate) struct EvalBarrier {
    reload_active: AtomicBool,
    idle: Notify,
}

impl EvalBarrier {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            reload_active: AtomicBool::new(false),
            idle: Notify::new(),
        })
    }

    pub(crate) fn begin_reload(self: &Arc<Self>) -> ReloadGuard {
        let was_active = self.reload_active.swap(true, Ordering::AcqRel);
        debug_assert!(!was_active, "reloads must be serialized by GhciManager");
        ReloadGuard(self.clone())
    }

    async fn wait_until_idle(&self) {
        loop {
            // Register before checking so a transition to idle cannot be missed.
            let notified = self.idle.notified();
            if !self.reload_active.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct ReloadGuard(Arc<EvalBarrier>);

impl Drop for ReloadGuard {
    fn drop(&mut self) {
        self.0.reload_active.store(false, Ordering::Release);
        self.0.idle.notify_waiters();
    }
}

/// Owns both the advisory lock and the socket pathname. The lock is still held
/// while `drop` removes the socket, so a successor cannot remove a new socket.
struct SocketLease {
    path: PathBuf,
    _lock: File,
}

impl Drop for SocketLease {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), %error, "Failed to remove eval socket");
            }
        }
    }
}

pub(crate) async fn spawn(
    ghci: Arc<Mutex<Ghci>>,
    barrier: Arc<EvalBarrier>,
) -> eyre::Result<()> {
    let directory = std::env::current_dir()?;
    let path = directory.join("ghciwatch-eval.sock");
    let lock_path = directory.join("ghciwatch-eval.lock");

    // `flock` waits without polling and the kernel releases it even after a
    // crash or SIGKILL. Do the blocking operation away from Tokio's workers.
    let lock = tokio::task::spawn_blocking(move || acquire_lock(&lock_path))
        .await
        .wrap_err("Eval socket lock task failed")??;

    // Only the lock owner may remove this path. It may remain after an
    // unclean shutdown, but cannot belong to a live cooperating process.
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::debug!(path = %path.display(), "Removed stale eval socket"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).wrap_err("Failed to remove stale ghciwatch-eval.sock"),
    }

    let listener = UnixListener::bind(&path).wrap_err("Failed to bind ghciwatch-eval.sock")?;
    let lease = SocketLease { path, _lock: lock };
    tokio::spawn(run(listener, lease, ghci, barrier));
    Ok(())
}

fn acquire_lock(path: &Path) -> eyre::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .wrap_err_with(|| format!("Failed to open {}", path.display()))?;
    flock(lock.as_raw_fd(), FlockArg::LockExclusive)
        .wrap_err_with(|| format!("Failed to lock {}", path.display()))?;
    Ok(lock)
}

async fn run(
    listener: UnixListener,
    _lease: SocketLease,
    ghci: Arc<Mutex<Ghci>>,
    barrier: Arc<EvalBarrier>,
) {
    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                let ghci = ghci.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(socket, ghci, barrier).await {
                        tracing::error!(?error, "Eval socket request failed");
                    }
                });
            }
            Err(error) => {
                // Transient resource errors must not permanently disable the
                // listener. Avoid a hot loop if an error persists.
                tracing::error!(%error, "Failed to accept eval socket connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(
    mut socket: UnixStream,
    ghci: Arc<Mutex<Ghci>>,
    barrier: Arc<EvalBarrier>,
) -> eyre::Result<()> {
    let mut request = Vec::new();
    {
        let mut reader = BufReader::new(&mut socket);
        loop {
            let byte = reader
                .read_u8()
                .await
                .wrap_err("Eval socket command ended before the ⋳ terminator")?;
            request.push(byte);
            eyre::ensure!(
                request.len() as u64 <= MAX_COMMAND_BYTES + COMMAND_TERMINATOR.len() as u64,
                "Eval socket command exceeds {MAX_COMMAND_BYTES} bytes"
            );
            if request.ends_with(COMMAND_TERMINATOR) {
                break;
            }
        }
    }
    request.truncate(request.len() - COMMAND_TERMINATOR.len());
    let command = std::str::from_utf8(&request).wrap_err("Eval socket command is not UTF-8")?;
    eyre::ensure!(!command.is_empty(), "Eval socket command is empty");

    let output = loop {
        barrier.wait_until_idle().await;
        let mut ghci = ghci.lock().await;

        // A reload may have become pending after wait_until_idle but before this
        // task acquired the mutex. Yield the mutex to the reload in that case.
        if barrier.reload_active.load(Ordering::Acquire) {
            drop(ghci);
            continue;
        }

        break eval_socket_command(&mut ghci, command)
            .await
            .wrap_err("Failed to evaluate socket command")?;
    };
    socket
        .write_all(output.as_bytes())
        .await
        .wrap_err("Failed to write eval socket response")?;
    socket
        .shutdown()
        .await
        .wrap_err("Failed to finish eval socket response")
}

async fn eval_socket_command(ghci: &mut Ghci, command: &str) -> eyre::Result<String> {
    // Synchronize the clear before writing: otherwise a fast syntax error could
    // be buffered and then erased by a delayed clear event.
    let (sender, receiver) = oneshot::channel();
    ghci.stdout
        .stderr_sender
        .send(StderrEvent::ClearBuffer { sender })
        .await?;
    receiver.await?;

    let command = command.trim_end();
    ghci.stdin
        .stdin
        .write_all(format!("{command}\n").as_bytes())
        .await?;

    // GHCi emits a prompt for every input line, including continuation prompts.
    let mut output = String::new();
    for _ in 0..command.lines().count() {
        output.push_str(
            &ghci
                .stdout
                .reader
                .read_until(&mut ReadOpts {
                    end_marker: &ghci.stdout.prompt_patterns,
                    find: FindAt::LineStart,
                    writing: WriteBehavior::NoFinalLine,
                    buffer: &mut ghci.stdout.buffer,
                })
                .await?,
        );
    }

    let (sender, receiver) = oneshot::channel();
    ghci.stdout
        .stderr_sender
        .send(StderrEvent::GetBuffer { sender })
        .await?;
    output.push_str(&receiver.await?);
    Ok(output)
}
