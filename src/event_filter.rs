//! Parsing [`DebouncedEvent`]s into changes `ghciwatch` can respond to.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::hash::Hasher;
use std::io::ErrorKind;
use std::io::Read;
use std::time::SystemTime;

use camino::Utf8Path;
use camino::Utf8PathBuf;
use notify_debouncer_full::notify::EventKind;
use notify_debouncer_full::DebouncedEvent;

use crate::haskell_source_file::is_haskell_source_file;

/// A set of filesystem events that `ghci` will need to respond to. Due to the way that `ghci` is,
/// we need to divide these into a few different classes so that we can respond appropriately.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileEvent {
    /// Existing files that are modified, or new files that are created.
    ///
    /// `inotify` APIs aren't great at distinguishing between newly-created files and modified
    /// existing files (particularly because some editors, like `vim`, will write to a temporary
    /// file and then move that file over the original for atomicity), so this includes both sorts
    /// of changes.
    Modify(Utf8PathBuf),
    /// A file is removed.
    Remove(Utf8PathBuf),
}

impl FileEvent {
    /// Get the contained path.
    pub fn as_path(&self) -> &Utf8Path {
        match self {
            FileEvent::Modify(path) => path.as_path(),
            FileEvent::Remove(path) => path.as_path(),
        }
    }
}

/// State of an event path when a debounced watcher batch was delivered.
///
/// Haskell files include a content hash so delayed notifications for an already-compiled edit can
/// be discarded without relying on filesystem timestamp granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    kind: FileStateKind,
    len: u64,
    modified: Option<SystemTime>,
    content_hash: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStateKind {
    Missing,
    File,
    Directory,
    Other,
}

impl FileState {
    fn capture(path: &Utf8Path) -> eyre::Result<Self> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self {
                    kind: FileStateKind::Missing,
                    len: 0,
                    modified: None,
                    content_hash: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let kind = if metadata.is_file() {
            FileStateKind::File
        } else if metadata.is_dir() {
            FileStateKind::Directory
        } else {
            FileStateKind::Other
        };
        let content_hash = if metadata.is_file() && is_haskell_source_file(path) {
            let mut file = match std::fs::File::open(path) {
                Ok(file) => file,
                // Atomic saves can replace a path between metadata and open. Treat that transient
                // observation as missing; a later watcher snapshot will carry the replacement.
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return Ok(Self {
                        kind: FileStateKind::Missing,
                        len: 0,
                        modified: None,
                        content_hash: None,
                    });
                }
                Err(error) => return Err(error.into()),
            };
            let mut hasher = DefaultHasher::new();
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.write(&buffer[..read]);
            }
            Some(hasher.finish())
        } else {
            None
        };
        Ok(Self {
            kind,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            content_hash,
        })
    }
}

/// Capture the latest state for every path represented by a watcher batch.
pub fn file_states(events: &BTreeSet<FileEvent>) -> eyre::Result<BTreeMap<Utf8PathBuf, FileState>> {
    events
        .iter()
        .map(|event| {
            let path = event.as_path();
            Ok((path.to_owned(), FileState::capture(path)?))
        })
        .collect()
}

/// Process a set of events into a set of [`FileEvent`]s.
pub fn file_events_from_action(events: Vec<DebouncedEvent>) -> eyre::Result<BTreeSet<FileEvent>> {
    let mut mutation_paths = BTreeSet::new();

    for event in events {
        let event = event.event;
        if matches!(event.kind, EventKind::Access(_)) {
            // Non-mutating event, ignore these.
            continue;
        }

        if matches!(
            event.kind,
            EventKind::Any
                | EventKind::Other
                | EventKind::Create(_)
                | EventKind::Modify(_)
                | EventKind::Remove(_)
        ) {
            for path in event.paths {
                mutation_paths.insert(Utf8PathBuf::try_from(path)?);
            }
        }
    }

    // A single debounce batch may contain remove/create/modify notifications for the same atomic
    // save. Classify each path once from its final state instead of dispatching contradictory
    // Remove and Modify hints.
    Ok(mutation_paths
        .into_iter()
        .map(|path| {
            if path.exists() {
                FileEvent::Modify(path)
            } else {
                FileEvent::Remove(path)
            }
        })
        .collect())
}
