use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use eyre::Context;
use nix::fcntl::{flock, FlockArg};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};

use crate::ghci::parse::{parse_ghc_messages, CompilationResult};
use crate::ghci::{CompilationLog, Ghci, StderrEvent};
use crate::incremental_reader::{FindAt, ReadOpts, WriteBehavior};

const MAX_COMMAND_BYTES: u64 = 1024 * 1024;
const COMMAND_TERMINATOR: &[u8] = "⋳".as_bytes();
const EVAL_WARNING_AFTER: Duration = Duration::from_secs(30);
const EVAL_TIMEOUT_AFTER_WARNING: Duration = Duration::from_secs(30);
static EVAL_NONCE: AtomicU64 = AtomicU64::new(0);

/// Serializes eval commands and reloads before either one acquires the GHCi mutex.
///
/// Tokio's mutex is FIFO, so once a reload is queued, later eval requests cannot
/// continually jump ahead of it. Holding the permit for the complete operation also
/// makes cancellation safe: dropping the operation releases the permit automatically.
pub(crate) struct EvalBarrier {
    gate: Arc<Mutex<()>>,
}

impl EvalBarrier {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) async fn begin_operation(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.gate.clone().lock_owned().await
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
    path: PathBuf,
) -> eyre::Result<()> {
    let lock_path = path.with_extension("lock");

    // The eval endpoint is optional when another ghciwatch session already owns it.
    // Never wait for that session: doing so would leave this session's initialized GHCi
    // manager unable to consume the file events produced by its watcher.
    let Some(lock) = tokio::task::spawn_blocking(move || try_acquire_lock(&lock_path))
        .await
        .wrap_err("Eval socket lock task failed")??
    else {
        tracing::info!(
            path = %path.display(),
            "Another ghciwatch session owns the eval socket; continuing without executable eval"
        );
        return Ok(());
    };

    // Only the lock owner may remove this path. It may remain after an
    // unclean shutdown, but cannot belong to a live cooperating process.
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::debug!(path = %path.display(), "Removed stale eval socket"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).wrap_err("Failed to remove stale eval socket"),
    }

    let listener = UnixListener::bind(&path)
        .wrap_err_with(|| format!("Failed to bind eval socket {}", path.display()))?;
    let lease = SocketLease { path, _lock: lock };
    tokio::spawn(run(listener, lease, ghci, barrier));
    Ok(())
}

fn try_acquire_lock(path: &Path) -> eyre::Result<Option<File>> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .wrap_err_with(|| format!("Failed to open {}", path.display()))?;
    match flock(lock.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(Some(lock)),
        Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
        Err(error) => Err(error).wrap_err_with(|| format!("Failed to lock {}", path.display())),
    }
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

    // Take the operation permit before the GHCi mutex. Reloads use the same
    // ordering, so an eval arriving during a reload sleeps without touching any
    // of GHCi's pipes and a reload arriving during an eval waits for it to finish.
    let operation_guard = barrier.begin_operation().await;
    let mut ghci = ghci.lock().await;
    let result = if is_reload_command(command) {
        ghci.reload_from_eval().await.map(|()| String::new())
    } else {
        eval_socket_command(&mut ghci, command).await
    };
    let output = match result.wrap_err("Failed to evaluate socket command") {
        Ok(output) => output,
        Err(error) => {
            tracing::error!(?error, "Eval socket command failed");
            format!("Error: ghciwatch executable eval failed: {error:#}")
        }
    };
    // The client may be slow or disappear. GHCi is free as soon as evaluation is
    // complete; socket delivery must not delay a queued reload or another eval.
    drop(ghci);
    drop(operation_guard);
    // An empty successful GHCi command is still a response. Send one delimiter in
    // that case so DelimitedEnd clients can distinguish it from EOF before response,
    // while preserving the historical EOF-terminated form for nonempty responses.
    let response = if output.is_empty() {
        COMMAND_TERMINATOR
    } else {
        output.as_bytes()
    };
    if let Err(error) = socket.write_all(response).await {
        if is_disconnected_client(&error) {
            tracing::debug!(%error, "Eval client disconnected before reading the response");
            return Ok(());
        }
        return Err(error).wrap_err("Failed to write eval socket response");
    }
    if let Err(error) = socket.shutdown().await {
        if is_disconnected_client(&error) {
            tracing::debug!(%error, "Eval client disconnected before response shutdown");
            return Ok(());
        }
        return Err(error).wrap_err("Failed to finish eval socket response");
    }
    Ok(())
}

/// A socket client is free to stop waiting while its eval is queued or running.
/// Failure to deliver that client's response does not affect GHCi or the listener.
fn is_disconnected_client(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

fn is_reload_command(command: &str) -> bool {
    matches!(command.trim(), ":r" | ":reload")
}

async fn eval_socket_command(ghci: &mut Ghci, command: &str) -> eyre::Result<String> {
    let command = command.trim_end();
    eyre::ensure!(!command.is_empty(), "Eval socket command is empty");

    // Time the complete pipe protocol, not only the main stdout read. A wedged
    // stderr clear/barrier is just as capable of blocking reloads indefinitely.
    let mut evaluation = Box::pin(eval_socket_command_inner(ghci, command));
    match tokio::time::timeout(EVAL_WARNING_AFTER, &mut evaluation).await {
        Ok(result) => result,
        Err(_) => {
            tracing::error!(
                "Ghciwatch executable eval error: Command \"{command}\" has taken more than 30 seconds! Waiting 30 more then I will attempt to interrupt it."
            );
            match tokio::time::timeout(EVAL_TIMEOUT_AFTER_WARNING, &mut evaluation).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::error!(
                        "Ghciwatch executable eval error: Command \"{command}\" has taken more than 60 seconds! Killing eval."
                    );
                    // Cancel every pending pipe/channel operation first. In particular,
                    // this closes the stderr marker response channel so that its task
                    // stops waiting for a marker which may never be emitted.
                    drop(evaluation);
                    ghci.send_sigint()
                        .await
                        .wrap_err("Failed to restore GHCi after timed-out executable eval")?;
                    tracing::info!(
                        "Ghciwatch executable eval: Successfully interrupted command \"{command}\" and restored the underlying ghciwatch GHCi session."
                    );
                    Err(eyre::eyre!(
                        "Executable eval command timed out after 60 seconds: {command}"
                    ))
                }
            }
        }
    }
}

async fn eval_socket_command_inner(ghci: &mut Ghci, command: &str) -> eyre::Result<String> {
    // Synchronize the clear before writing: otherwise a fast syntax error could
    // be buffered and then erased by a delayed clear event.
    let (sender, receiver) = oneshot::channel();
    ghci.stdout
        .stderr_sender
        .send(StderrEvent::ClearBuffer { sender })
        .await?;
    receiver.await?;

    // Submit and synchronize one line at a time. Writing the complete command and
    // merely counting prompts is not safe: a parse/type error can terminate a
    // continuation early, leaving prompts buffered and desynchronizing every later
    // operation on this GHCi session.
    let mut output = String::new();
    for line in command.lines() {
        ghci.stdin
            .stdin
            .write_all(format!("{line}\n").as_bytes())
            .await?;
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

        // A failed `:module` means later imports and expressions can only add
        // misleading "not in scope" errors. Synchronize stderr now and stop the
        // request at the primary load failure instead.
        if matches!(line.split_whitespace().next(), Some(":m" | ":module")) {
            let stderr = stderr_through_marker(ghci).await?;
            let mut log = CompilationLog::default();
            log.extend(parse_ghc_messages(&stderr)?);
            log.fill_empty_summary();
            let module_failed = log.result() == Some(CompilationResult::Err);
            output.push_str(&stderr);
            if module_failed {
                return Ok(output);
            }

            // The synchronized output has already been copied into the response.
            // Start a fresh stderr segment for the remaining request lines.
            let (sender, receiver) = oneshot::channel();
            ghci.stdout
                .stderr_sender
                .send(StderrEvent::ClearBuffer { sender })
                .await?;
            receiver.await?;
        }
    }

    output.push_str(&stderr_through_marker(ghci).await?);
    Ok(output)
}

/// Drain stderr through a marker emitted after all preceding GHCi work.
async fn stderr_through_marker(ghci: &mut Ghci) -> eyre::Result<String> {
    // stdout and stderr are independent pipes. Seeing the prompt on stdout does
    // not prove that the stderr task has consumed the diagnostic yet.
    let nonce = EVAL_NONCE.fetch_add(1, Ordering::Relaxed);
    let marker = format!("__GHCIWATCH_EVAL_END_{}_{}__", std::process::id(), nonce);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let (sender, receiver) = oneshot::channel();
    ghci.stdout
        .stderr_sender
        .send(StderrEvent::GetBufferThrough {
            marker: marker.clone(),
            ready: ready_sender,
            sender,
        })
        .await?;
    // Do not emit the marker until the stderr task is definitely waiting for it.
    ready_receiver.await?;

    ghci.stdin
        .stdin
        .write_all(format!(":! printf '%s\\n' '{marker}' >&2\n").as_bytes())
        .await?;
    // Consume the marker command's stdout prompt as well as its stderr marker. The shell
    // command intentionally emits no stdout, so GHCi may print this prompt immediately after
    // the previously consumed prompt with no intervening newline.
    let _ = ghci
        .stdout
        .reader
        .read_until(&mut ReadOpts {
            end_marker: &ghci.stdout.prompt_patterns,
            find: FindAt::Anywhere,
            writing: WriteBehavior::NoFinalLine,
            buffer: &mut ghci.stdout.buffer,
        })
        .await?;
    Ok(receiver.await?)
}
