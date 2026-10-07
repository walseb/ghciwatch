//! Bounded compilation memory history. Never sends a GHCi command or forces GC.
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::memory::repl_resident_memory;

mod major_gc;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Workload {
    /// Operation through completion hooks; excludes subsequent eval/test actions.
    pub duration_seconds: f64,
    /// Observed Compiling messages, including failed attempts and repeated modules.
    pub module_compile_attempts: u64,
    /// Watcher event paths, including removals and explicit reload triggers.
    pub changed_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RtsSample {
    pub pid: u32,
    pub gcs: u32,
    pub major_gcs: u32,
    pub last_gc_generation: u32,
    pub live_bytes: u64,
    pub heap_bytes: u64,
    pub allocated_bytes: u64,
    pub gc_allocated_bytes: u64,
    pub max_live_bytes: u64,
    pub gc_cpu_ns: u64,
    pub gc_elapsed_ns: u64,
    pub process_rss_bytes: u64,
    pub process_swap_bytes: u64,
    pub process_lazy_free_bytes: u64,
}

pub(super) fn capture(output: &str, sample: &mut Option<RtsSample>) {
    for line in output.lines() {
        if let Some(payload) = line.strip_prefix("GHCIWATCH_MEMORY ") {
            // A malformed newer sample must not leave a previous one looking current.
            *sample = match serde_json::from_str(payload) {
                Ok(value) => Some(value),
                Err(error) => {
                    tracing::warn!(%error, "Invalid lifecycle RTS memory sample");
                    None
                }
            };
        }
    }
}

pub(super) struct MemoryLog {
    path: Option<Utf8PathBuf>,
    session: String,
    generation: u64,
}

impl MemoryLog {
    pub(super) fn new(path: Option<Utf8PathBuf>, command_pid: i32) -> Self {
        Self {
            path,
            session: format!("{}-{command_pid}-{}", std::process::id(), unix_ns()),
            generation: 0,
        }
    }

    pub(super) async fn write(
        &mut self,
        command_pid: i32,
        process_group_id: i32,
        outcome: &'static str,
        sample: Option<RtsSample>,
        workload: Workload,
    ) -> eyre::Result<()> {
        let generation = self.generation;
        self.generation += 1;
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || -> eyre::Result<()> {
            let mut major = Err(eyre::eyre!("Persistent GHC unavailable"));
            let (os, os_error) = match repl_resident_memory(command_pid, process_group_id) {
                Ok(usage) if usage.interactive_ghc.is_some() => {
                    major = major_gc::read(usage.interactive_ghc.unwrap().0);
                    (
                        json!({
                            "ghc_pid": usage.interactive_ghc.map(|(pid, _)| pid),
                            "cabal_pid": usage.cabal_parent.map(|(pid, _)| pid),
                            "rss_bytes": usage.bytes,
                            "lazy_free_bytes": usage.lazy_free_bytes,
                            "charged_bytes": usage.charged_bytes(),
                            "swap_bytes": usage.swap_bytes,
                        }),
                        None,
                    )
                }
                Ok(_) => (
                    serde_json::Value::Null,
                    Some("Persistent GHC/Cabal pair unavailable".to_owned()),
                ),
                Err(error) => (serde_json::Value::Null, Some(error.to_string())),
            };
            let (major_gc, major_gc_error) = match major {
                Ok(sample) => (Some(sample), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let assessment = match sample.as_ref() {
                None => "unavailable",
                Some(stats) if stats.last_gc_generation == 0 => "minor_gc_inconclusive",
                Some(_) => "major_gc_snapshot",
            };
            let record = json!({
                "version": 2,
                "workload": workload,
                "major_gc": major_gc,
                "major_gc_error": major_gc_error,
                "timestamp_unix_ns": unix_ns(),
                "session": session,
                "command_pid": command_pid,
                "generation": generation,
                "outcome": outcome,
                "rts_status": assessment,
                "rts": sample,
                "os": os,
                "os_error": os_error,
            });
            if let Some(parent) = path.parent().filter(|parent| !parent.as_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let mut line = serde_json::to_vec(&record)?;
            line.push(b'\n');
            append(&path, &line)
        })
        .await??;
        Ok(())
    }
}

const PRUNE_BYTES: u64 = 15 * 1024 * 1024;
const KEEP_BYTES: u64 = 10 * 1024 * 1024;

fn append(path: &camino::Utf8Path, line: &[u8]) -> eyre::Result<()> {
    eyre::ensure!(
        line.len() as u64 <= KEEP_BYTES,
        "Memory history record exceeds retention size"
    );
    // Lock a stable inode BEFORE opening the data file: rename would otherwise let
    // another writer append to an obsolete inode. Never remove this lock file.
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{path}.lock"))?;
    nix::fcntl::flock(lock.as_raw_fd(), nix::fcntl::FlockArg::LockExclusive)?;
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    file.write_all(line)?;
    let len = file.metadata()?.len();
    if len >= PRUNE_BYTES {
        // Read one extra byte so an exactly aligned record boundary is retained.
        file.seek(SeekFrom::Start(len - KEEP_BYTES - 1))?;
        let mut tail = Vec::with_capacity(KEEP_BYTES as usize + 1);
        file.take(KEEP_BYTES + 1).read_to_end(&mut tail)?;
        let start = tail
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or_else(|| eyre::eyre!("Memory history tail has no record boundary"))?
            + 1;
        // A fixed sibling is safe under the stable lock; truncate leftovers from
        // interrupted pruning. Readers see either the old file or the complete tail.
        let temp = format!("{path}.prune");
        let result = (|| -> eyre::Result<()> {
            let mut out = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temp)?;
            out.write_all(&tail[start..])?;
            out.sync_all()?;
            drop(out);
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
    }
    Ok(())
}

fn unix_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System clock predates Unix epoch")
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn appends_generations_and_preserves_sessions_without_stale_samples() {
        let path = Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!(
            "ghciwatch-memory-{}-{}.jsonl",
            std::process::id(),
            unix_ns()
        )))
        .unwrap();
        let mut log = MemoryLog::new(Some(path.clone()), -1);
        log.write(-1, -1, "success", None, Workload::default())
            .await
            .unwrap();
        log.write(-1, -1, "failed", None, Workload::default())
            .await
            .unwrap();
        let mut restarted = MemoryLog::new(Some(path.clone()), -2);
        restarted
            .write(-2, -2, "success", None, Workload::default())
            .await
            .unwrap();
        let records: Vec<serde_json::Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["generation"], 0);
        assert_eq!(records[1]["generation"], 1);
        assert_eq!(records[2]["generation"], 0);
        assert_eq!(records[0]["session"], records[1]["session"]);
        assert_ne!(records[0]["session"], records[2]["session"]);
        assert_eq!(records[1]["outcome"], "failed");
        assert_eq!(records[1]["rts_status"], "unavailable");
        assert!(records[1]["rts"].is_null());
        fs::remove_file(&path).unwrap();
        fs::remove_file(format!("{path}.lock")).unwrap();
    }

    #[test]
    fn prunes_complete_records_and_serializes_writers() {
        let dir = std::env::temp_dir().join(format!(
            "ghciwatch-prune-{}-{}",
            std::process::id(),
            unix_ns()
        ));
        fs::create_dir(&dir).unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.join("memory.txt")).unwrap();
        // Reach the real production threshold with short, fixed-length JSON lines.
        // The retained window begins inside a record, exercising boundary trimming.
        let line = b"{\"n\":12345}\n";
        let count = (PRUNE_BYTES as usize - 1) / line.len();
        fs::write(&path, line.repeat(count)).unwrap();
        append(&path, line).unwrap();
        let contents = fs::read(&path).unwrap();
        assert!(contents.len() <= KEEP_BYTES as usize);
        assert!(contents.len() > KEEP_BYTES as usize - line.len());
        assert_eq!(contents, line.repeat(contents.len() / line.len()));
        // An already aligned boundary must not discard another record.
        fs::write(&path, b"0\n".repeat(PRUNE_BYTES as usize / 2 - 1)).unwrap();
        append(&path, b"0\n").unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"0\n".repeat(KEEP_BYTES as usize / 2)
        );
        // Race independent writers across another replacement: all their new
        // records must survive, including writers that were waiting on the lock.
        fs::write(&path, line.repeat(count)).unwrap();
        std::thread::scope(|scope| {
            for writer in 0..4 {
                let path = &path;
                scope.spawn(move || {
                    for generation in 0..20 {
                        append(
                            path,
                            format!("{{\"writer\":{writer},\"generation\":{generation}}}\n")
                                .as_bytes(),
                        )
                        .unwrap();
                    }
                });
            }
        });
        let contents = fs::read_to_string(&path).unwrap();
        let mut seen = std::collections::HashSet::new();
        for line in contents.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record.get("writer").is_some() {
                assert!(seen.insert((
                    record["writer"].as_u64().unwrap(),
                    record["generation"].as_u64().unwrap()
                )));
            }
        }
        assert_eq!(seen.len(), 80);
        assert!(!std::path::Path::new(&format!("{path}.prune")).exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
