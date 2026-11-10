//! Local filesystem content store.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use super::validate_key;
use crate::StoreError;

/// A content store rooted at a local directory.
///
/// Writes are atomic: bytes are written to a temporary file in the destination
/// directory and then renamed into place. Reads are plain `std::fs` operations
/// and block the calling task; the same is true for writes and `put_file`
/// copies.
///
/// The root directory must not be writable by untrusted users. Keys are
/// validated to reject absolute paths and `..` components, but filesystem
/// symlinks created by a third party inside the root are outside the threat
/// model.
#[derive(Clone, Debug)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    /// Creates a store rooted at `root`, creating the directory when needed.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the directory cannot be created or resolved,
    /// or [`StoreError::InvalidConfig`] when `root` is not a directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let root = fs::canonicalize(&root)?;
        if !root.is_dir() {
            return Err(StoreError::InvalidConfig {
                message: format!("{} is not a directory", root.display()),
            });
        }
        Ok(Self { root })
    }

    /// Returns the canonical root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes `bytes` to `key`, replacing any existing file.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or an I/O error
    /// when the temporary file cannot be written or persisted.
    pub fn put_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.put_with(key, |file| file.write_all(bytes))
    }

    /// Copies `path` to `key`, replacing any existing file.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or an I/O error
    /// when the source cannot be read or the destination cannot be written.
    pub fn put_file(&self, key: &str, path: &Path) -> Result<(), StoreError> {
        let mut source = File::open(path)?;
        self.put_with(key, |file| std::io::copy(&mut source, file).map(|_| ()))
    }

    /// Reads `range` (exclusive end) from `key`.
    ///
    /// Ranges are clamped to the file length; a range that starts at or after
    /// the end returns an empty vector.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the file does not exist, or an I/O
    /// error when it cannot be read.
    pub fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, StoreError> {
        let path = self.resolve(key)?;
        let mut file = File::open(&path).map_err(|error| io_error(key, error))?;
        let len = file.metadata()?.len();
        let start = range.start.min(len);
        let end = range.end.min(len);
        if end <= start {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(start))?;
        let length = usize::try_from(end - start).map_err(|_| StoreError::InvalidConfig {
            message: "requested range does not fit in memory".to_owned(),
        })?;
        let mut bytes = vec![0_u8; length];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Reads the complete file at `key`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the file does not exist, or an I/O
    /// error when it cannot be read.
    pub fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.resolve(key)?;
        fs::read(&path).map_err(|error| io_error(key, error))
    }

    /// Returns the file length in bytes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the file does not exist, or an I/O
    /// error when its metadata cannot be read.
    pub fn object_size(&self, key: &str) -> Result<u64, StoreError> {
        let path = self.resolve(key)?;
        let metadata = fs::metadata(&path).map_err(|error| io_error(key, error))?;
        Ok(metadata.len())
    }

    /// Returns whether `key` exists and is a file.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or an I/O error
    /// other than "not found" when metadata cannot be read.
    pub fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let path = self.resolve(key)?;
        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    fn put_with(
        &self,
        key: &str,
        write: impl FnOnce(&mut File) -> std::io::Result<()>,
    ) -> Result<(), StoreError> {
        let path = self.resolve(key)?;
        let parent = path.parent().ok_or_else(|| StoreError::InvalidKey {
            key: key.to_owned(),
        })?;
        fs::create_dir_all(parent)?;

        let mut temp = NamedTempFile::new_in(parent)?;
        write(temp.as_file_mut())?;
        temp.as_file_mut().flush()?;
        temp.persist(&path)
            .map_err(|error| StoreError::Io(error.error))?;
        Ok(())
    }

    fn resolve(&self, key: &str) -> Result<PathBuf, StoreError> {
        validate_key(key)?;
        let path = self.root.join(key);
        if !path.starts_with(&self.root) {
            return Err(StoreError::InvalidKey {
                key: key.to_owned(),
            });
        }
        Ok(path)
    }
}

fn io_error(key: &str, error: std::io::Error) -> StoreError {
    if error.kind() == std::io::ErrorKind::NotFound {
        StoreError::NotFound {
            key: key.to_owned(),
        }
    } else {
        StoreError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::LocalStore;
    use crate::StoreError;

    fn store() -> (tempfile::TempDir, LocalStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::new(directory.path().join("root")).unwrap();
        (directory, store)
    }

    #[test]
    fn put_get_and_range_reads_round_trip() {
        let (_directory, store) = store();
        store
            .put_bytes("objects/abc", b"0123456789")
            .expect("write succeeds");

        assert_eq!(store.get("objects/abc").unwrap(), b"0123456789");
        assert_eq!(store.object_size("objects/abc").unwrap(), 10);
        assert!(store.exists("objects/abc").unwrap());
        assert_eq!(
            store.get_range("objects/abc", 2..5).unwrap(),
            b"234".to_vec()
        );
        assert_eq!(
            store.get_range("objects/abc", 8..100).unwrap(),
            b"89".to_vec()
        );
        assert_eq!(
            store.get_range("objects/abc", 20..30).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            store.get_range("objects/abc", 7..7).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn missing_objects_are_reported_as_not_found() {
        let (_directory, store) = store();

        assert!(!store.exists("objects/missing").unwrap());
        assert!(matches!(
            store.get("objects/missing"),
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            store.get_range("objects/missing", 0..10),
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            store.object_size("objects/missing"),
            Err(StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn parent_directories_are_created() {
        let (_directory, store) = store();
        store
            .put_bytes("packs/fetch/2026-11-10T00/1.pack", b"pack")
            .unwrap();

        assert_eq!(
            store.get("packs/fetch/2026-11-10T00/1.pack").unwrap(),
            b"pack"
        );
    }

    #[test]
    fn traversal_keys_are_rejected_without_touching_the_filesystem() {
        let (directory, store) = store();
        let outside = directory.path().join("outside");

        for key in ["../outside", "a/../../outside", "/tmp/outside"] {
            assert!(
                matches!(
                    store.put_bytes(key, b"escape"),
                    Err(StoreError::InvalidKey { .. })
                ),
                "{key}"
            );
            assert!(matches!(store.get(key), Err(StoreError::InvalidKey { .. })));
        }
        assert!(!outside.exists());
    }

    #[test]
    fn put_file_copies_the_source() {
        let (directory, store) = store();
        let source = directory.path().join("source.bin");
        let mut file = std::fs::File::create(&source).unwrap();
        file.write_all(b"file contents").unwrap();
        drop(file);

        store.put_file("objects/file", &source).unwrap();
        assert_eq!(store.get("objects/file").unwrap(), b"file contents");
    }

    #[test]
    fn writes_replace_existing_objects() {
        let (_directory, store) = store();
        store.put_bytes("objects/replace", b"old").unwrap();
        store.put_bytes("objects/replace", b"new").unwrap();

        assert_eq!(store.get("objects/replace").unwrap(), b"new");
    }
}
