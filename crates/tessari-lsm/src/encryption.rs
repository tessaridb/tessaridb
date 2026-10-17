//! An encrypted store (ADR-0108 D7): every file the engine writes is encrypted
//! under the engine subkey of the operator's key.
//!
//! # How
//!
//! The engine seals each file it writes under a key of its own, derived from the
//! root key the store hands it (the engine subkey). Integrity is the engine's own:
//! a sealed file that does not open under its key is refused, never read.
//!
//! # Which store is which
//!
//! A file named [`MARKER`] beside the engine's files says the store is
//! encrypted, and it is sealed under the key's check subkey, so opening it is
//! how a key is known to be the right one before the engine reads anything.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use tessari_kv::Error;
use tessari_vault::AtRestKey;

use crate::error::BACKEND_NAME;

/// The file that marks a store as encrypted.
pub(crate) const MARKER: &str = "ENCRYPTION";

pub(crate) fn admit(path: &Path, key: Option<&AtRestKey>, exists: bool) -> Result<(), Error> {
    let marker = path.join(MARKER);
    let marked = marker.exists();
    match (key, marked) {
        (None, false) => Ok(()),
        (None, true) => Err(Error::validation(format!(
            "the store at {} is encrypted; it opens only with its key (--encryption-key-file)",
            path.display()
        ))),
        (Some(_), false) if exists => Err(Error::validation(format!(
            "the store at {} is not encrypted, and an encryption key was given; \
             open it without one, or restore a backup of it into a new encrypted store",
            path.display()
        ))),
        (Some(key), false) => write_marker(path, &marker, key),
        (Some(key), true) => {
            let held = fs::read(&marker).map_err(|failure| unavailable(&marker, &failure))?;
            key.opens(&held).map_err(|_| {
                Error::validation(format!(
                    "the encryption key does not open the store at {}: \
                     it was encrypted under another key",
                    path.display()
                ))
            })
        }
    }
}

/// Write the marker for a store about to be created, durably, before the
/// engine writes anything.
fn write_marker(folder: &Path, marker: &Path, key: &AtRestKey) -> Result<(), Error> {
    fs::create_dir_all(folder).map_err(|failure| unavailable(folder, &failure))?;
    let bytes = key.marker().map_err(|failure| Error::Unavailable {
        backend: BACKEND_NAME,
        reason: failure.to_string(),
    })?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .map_err(|failure| unavailable(marker, &failure))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|failure| unavailable(marker, &failure))?;
    fs::File::open(folder)
        .and_then(|held| held.sync_all())
        .map_err(|failure| unavailable(folder, &failure))
}

fn unavailable(path: &Path, failure: &std::io::Error) -> Error {
    Error::Unavailable {
        backend: BACKEND_NAME,
        reason: format!("{}: {failure}", path.display()),
    }
}
