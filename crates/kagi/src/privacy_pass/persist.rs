//! Durable state for unspent Privacy Pass tokens.
//!
//! The state file is a small JSON document holding the current queue in consumption order:
//!
//! ```json
//! {"version":1,"tokens":["<url-safe base64 token>","..."]}
//! ```
//!
//! Reads and writes are synchronous and happen while the token store lock is held. The files
//! are small and local, so the brief block is acceptable; no mutex is held across an await.
//! Writes go to a temporary file in the same directory, are flushed with `sync_all`, and are
//! renamed over the state file, so a crash cannot leave a partially written store behind.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use serde::{Deserialize, Serialize};

use super::token::PrivacyToken;

/// The current state file schema version.
const STATE_VERSION: u32 = 1;

/// Distinguishes temporary files written by one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The serialized state file.
#[derive(Debug, Deserialize, Serialize)]
struct StateFile {
    /// The schema version, so a future format change can be rejected cleanly.
    version: u32,
    /// The unspent tokens in consumption order, encoded as URL-safe base64.
    tokens: Vec<String>,
}

/// Errors that can occur while loading or saving the token state.
#[derive(Debug, thiserror::Error)]
pub(super) enum StateError {
    /// The state file could not be read or written.
    #[error("could not access the privacy pass state file: {0}")]
    Io(#[source] io::Error),
    /// The state file is not valid JSON.
    #[error("the privacy pass state file is not valid JSON: {0}")]
    Json(#[source] serde_json::Error),
    /// The state file was written by an unsupported version.
    #[error("unsupported privacy pass state version {0}")]
    Version(u32),
    /// The state file contains a token that does not match the wire format.
    #[error("the privacy pass state file contains an invalid token")]
    Token,
}

/// Loads the persisted token queue, or `None` when the file does not exist.
///
/// An unreadable or corrupt file is reported as an error so the caller can log it and start
/// with an empty store. The file is left untouched until the next successful save.
pub(super) fn load(path: &Path) -> Result<Option<Vec<PrivacyToken>>, StateError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::Io(error)),
    };

    // Tokens are bearer-like credentials: never leave a pre-existing file group/world readable,
    // even before the next atomic replacement.
    restrict_file(path);

    let state: StateFile = serde_json::from_slice(&bytes).map_err(StateError::Json)?;

    if state.version != STATE_VERSION {
        return Err(StateError::Version(state.version));
    }

    let mut tokens = Vec::with_capacity(state.tokens.len());

    for encoded in state.tokens {
        let bytes = URL_SAFE.decode(encoded).map_err(|_| StateError::Token)?;
        let token = PrivacyToken::deserialize(&bytes).ok_or(StateError::Token)?;

        tokens.push(token);
    }

    Ok(Some(tokens))
}

/// Persists the token queue, replacing the state file atomically.
pub(super) fn save(path: &Path, tokens: &VecDeque<PrivacyToken>) -> Result<(), StateError> {
    let state = StateFile {
        version: STATE_VERSION,
        tokens: tokens
            .iter()
            .map(|token| URL_SAFE.encode(token.serialize()))
            .collect(),
    };
    let mut bytes = serde_json::to_vec(&state).map_err(StateError::Json)?;

    bytes.push(b'\n');
    write_atomic(path, &bytes).map_err(StateError::Io)
}

/// Writes `bytes` to a temporary file in the target directory and renames it into place.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let Some(file_name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the state path has no file name",
        ));
    };
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };

    create_private_dir(parent)?;

    let temporary = parent.join(temporary_name(file_name));
    let result = write_and_rename(&temporary, path, bytes);

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }

    result
}

/// Returns a unique temporary file name in the target directory.
fn temporary_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut name = std::ffi::OsString::from(".");

    name.push(file_name);
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    name
}

/// Writes, syncs, and renames the temporary file over the state file.
fn write_and_rename(temporary: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = open_private_file(temporary)?;

    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, path)?;
    sync_directory(path);

    Ok(())
}

/// Creates the state directory, restricting directories it creates to the current user.
///
/// An existing directory is used as-is: an operator may deliberately place the state file in
/// a directory with different permissions, and changing it would be surprising.
fn create_private_dir(directory: &Path) -> io::Result<()> {
    if directory.is_dir() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        let mut builder = std::fs::DirBuilder::new();

        builder.recursive(true).mode(0o700);
        builder.create(directory)
    }

    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory)
    }
}

/// Opens a new temporary state file readable and writable only by the current user.
#[cfg(unix)]
fn open_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Opens a new temporary state file.
#[cfg(not(unix))]
fn open_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Restricts a state file to the current user.
#[cfg(unix)]
fn restrict_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Leaves file permissions unchanged on platforms without Unix modes.
#[cfg(not(unix))]
fn restrict_file(_path: &Path) {}

/// Flushes the directory entry for the renamed file, best effort.
#[cfg(unix)]
fn sync_directory(path: &Path) {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return;
    };

    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

/// Directory syncing is not available on this platform.
#[cfg(not(unix))]
fn sync_directory(_path: &Path) {}
