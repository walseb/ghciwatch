//! Content-only filtering shared by setup notifications and reload classification.
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;

use camino::{Utf8Path, Utf8PathBuf};

use crate::event_filter::{FileEvent, FileState};
use crate::normal_path::NormalPath;

type Snapshot = BTreeMap<Utf8PathBuf, FileState>;

/// Last observed contents of watched regular files, including non-Haskell inputs.
/// Seed before launch so the first unchanged save is filtered too.
pub(super) struct ContentChanges {
    files: Snapshot,
}

impl ContentChanges {
    pub(super) fn new(roots: &[NormalPath]) -> eyre::Result<Self> {
        let mut files = Snapshot::new();
        for root in roots {
            scan(root.absolute(), &mut files)?;
        }
        Ok(Self { files })
    }

    pub(super) fn observe(
        &mut self,
        events: BTreeSet<FileEvent>,
    ) -> eyre::Result<BTreeSet<FileEvent>> {
        let mut changed = BTreeSet::new();
        for event in events {
            let path = event.as_path();
            // A directory notification may be the only hint for a whole-tree rename/removal.
            // Expand it to actual files; directory metadata alone never triggers work.
            let mut current = Snapshot::new();
            scan(path, &mut current)?;
            let previous_paths: Vec<_> = self
                .files
                .keys()
                .filter(|file| file.starts_with(path))
                .cloned()
                .collect();
            for file in previous_paths {
                let previous = self.files.remove(&file).unwrap();
                match current.remove(&file) {
                    Some(state) => {
                        if state != previous {
                            changed.insert(FileEvent::Modify(file.clone()));
                        }
                        self.files.insert(file, state);
                    }
                    None => {
                        changed.insert(FileEvent::Remove(file));
                    }
                }
            }
            for (file, state) in current {
                changed.insert(FileEvent::Modify(file.clone()));
                self.files.insert(file, state);
            }
        }
        Ok(changed)
    }
}

fn scan(path: &Utf8Path, files: &mut Snapshot) -> eyre::Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            // Do not recurse through directory symlinks (which may point back to an ancestor).
            if entry.file_type()?.is_symlink() && entry.path().is_dir() {
                continue;
            }
            scan(&Utf8PathBuf::try_from(entry.path())?, files)?;
        }
    } else if metadata.is_file() {
        files.insert(path.to_owned(), FileState::capture(path)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    struct Directory(Utf8PathBuf);
    impl Directory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = Utf8PathBuf::try_from(
                std::env::temp_dir()
                    .join(format!("ghciwatch-content-{}-{nonce}", std::process::id())),
            )
            .unwrap();
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn baseline(&self) -> ContentChanges {
            ContentChanges::new(&[NormalPath::from_cwd(&self.0).unwrap()]).unwrap()
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn event(path: &Utf8Path) -> BTreeSet<FileEvent> {
        BTreeSet::from([FileEvent::Modify(path.to_owned())])
    }

    #[test]
    fn contents_not_timestamps_for_all_extensions() {
        let directory = Directory::new();
        for name in ["Module.hs", "package.cabal", "README.md"] {
            let path = directory.0.join(name);
            fs::write(&path, "first").unwrap();
        }
        let mut changes = directory.baseline();
        for name in ["Module.hs", "package.cabal", "README.md"] {
            let path = directory.0.join(name);
            let original = FileState::capture(&path).unwrap();
            // Even the first notification must compare against the initial baseline.
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42)),
                )
                .unwrap();
            assert_eq!(original, FileState::capture(&path).unwrap());
            assert!(changes.observe(event(&path)).unwrap().is_empty());
            fs::write(&path, "first").unwrap();
            assert!(changes.observe(event(&path)).unwrap().is_empty());
            let temporary = directory.0.join("replacement");
            fs::write(&temporary, "first").unwrap();
            fs::rename(temporary, &path).unwrap();
            assert!(changes.observe(event(&path)).unwrap().is_empty());

            // Same-length edit with the very same timestamp must still be detected.
            let times =
                fs::FileTimes::new().set_modified(fs::metadata(&path).unwrap().modified().unwrap());
            fs::write(&path, "other").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(times)
                .unwrap();
            assert_eq!(changes.observe(event(&path)).unwrap(), event(&path));
            assert!(changes.observe(event(&path)).unwrap().is_empty());
            // Returning to the original contents is another genuine change.
            fs::write(&path, "first").unwrap();
            assert_eq!(changes.observe(event(&path)).unwrap(), event(&path));
        }
    }

    #[test]
    fn directory_events_detect_added_and_removed_files_not_directory_metadata() {
        let directory = Directory::new();
        let mut changes = directory.baseline();
        let nested = directory.0.join("nested");
        fs::create_dir(&nested).unwrap();
        assert!(changes.observe(event(&directory.0)).unwrap().is_empty());
        let path = nested.join("New.hs");
        fs::write(&path, "module New where").unwrap();
        assert_eq!(changes.observe(event(&nested)).unwrap(), event(&path));
        assert!(changes.observe(event(&nested)).unwrap().is_empty());
        fs::remove_dir_all(&nested).unwrap();
        let removal = BTreeSet::from([FileEvent::Remove(path.clone())]);
        assert_eq!(changes.observe(event(&nested)).unwrap(), removal);
        assert!(changes.observe(event(&nested)).unwrap().is_empty());
        fs::create_dir(&nested).unwrap();
        fs::write(&path, "module New where").unwrap();
        assert_eq!(changes.observe(event(&nested)).unwrap(), event(&path));
    }
}
