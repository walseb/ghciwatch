//! Passive evidence from the selected GHC's existing RTS -S file, never a new collection.
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub(super) struct Sample {
    pub live_bytes: u64,
    /// RTS elapsed time, not wall-clock time. The file can lag due to buffering.
    pub elapsed_seconds: f64,
    pub generation: u32,
}

pub(super) fn read(pid: i32) -> eyre::Result<Sample> {
    let cmd = fs::read(format!("/proc/{pid}/cmdline"))?;
    let mut rts = false;
    let mut path = None;
    let mut generations = 2_u32;
    for arg in cmd.split(|&byte| byte == 0) {
        match arg {
            b"+RTS" => rts = true,
            b"-RTS" | b"--RTS" => rts = false,
            _ if rts => {
                if let Some(value) = arg.strip_prefix(b"-S") {
                    path = (!value.is_empty()).then_some(value);
                }
                if let Some(value) = arg.strip_prefix(b"-G") {
                    generations = std::str::from_utf8(value)?.parse()?;
                }
            }
            _ => {}
        }
    }
    eyre::ensure!(generations > 0, "Invalid RTS generation count");
    let path = path.ok_or_else(|| eyre::eyre!("GHC has no explicit RTS -S file"))?;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        fs::read_link(format!("/proc/{pid}/cwd"))?.join(path)
    };
    let mut file = fs::File::open(path)?;
    // Bound per-reload I/O and allocation even if the RTS log grows indefinitely.
    // An absent major row is explicitly unavailable, never replaced with a prior session.
    let len = file.metadata()?.len();
    let start = len.saturating_sub(1024 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes)?;
    let text = std::str::from_utf8(&bytes)?;
    latest(text, start != 0, generations - 1)
        .ok_or_else(|| eyre::eyre!("No complete major-GC row in the last 1 MiB of RTS output"))
}

fn latest(text: &str, partial_start: bool, oldest: u32) -> Option<Sample> {
    text.split_inclusive('\n')
        .skip(usize::from(partial_start))
        .filter(|line| line.ends_with('\n'))
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 11 || fields[9] != "(Gen:" {
                return None;
            }
            let generation = fields[10].strip_suffix(')')?.parse::<u32>().ok()?;
            if generation != oldest {
                return None;
            }
            let live_bytes = fields[2].parse().ok()?;
            let elapsed_seconds = fields[6].parse::<f64>().ok()?;
            (elapsed_seconds.is_finite() && elapsed_seconds >= 0.0).then_some(Sample {
                live_bytes,
                elapsed_seconds,
                generation,
            })
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_latest_major_from_minor_and_incomplete_rows() {
        let rows = "header\n1 2 300 0.1 0.1 1.0 2.0 0 0 (Gen:  1)\n1 2 400 0.1 0.1 1.0 3.0 0 0 (Gen:  1)\n1 2 900 0.1 0.1 1.0 4.0 0 0 (Gen:  0)\n1 2 999 0.1 0.1 1.0 5.0 0 0 (Gen:  1)";
        let sample = latest(rows, false, 1).unwrap();
        assert_eq!(sample.live_bytes, 400);
        assert_eq!(sample.elapsed_seconds, 3.0);
        assert!(latest(rows, false, 2).is_none());
        assert!(latest("1 2 300 0.1 0.1 1.0 2.0 0 0 (Gen:  1)\n", true, 1).is_none());
    }
}
