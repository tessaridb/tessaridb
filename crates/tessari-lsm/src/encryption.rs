//! An encrypted store (ADR-0108 D7): every file the engine writes is encrypted
//! under the engine subkey of the operator's key.
//!
//! # How
//!
//! The engine runs on an environment whose files carry a 4 KiB clear prefix
//! holding a random 24-byte nonce, and whose bytes after it are XORed with the
//! XChaCha20 keystream of (engine subkey, that nonce) at their offset. The
//! adapter to the engine's C++ interface is `cipher.cc`; the keystream and the
//! nonces come from here. This is the one module in the crate that is allowed
//! `unsafe`, and every use of it is the boundary to that adapter.
//!
//! # What the stream relies on
//!
//! A keystream position must never encrypt two different bytes. It holds
//! because every file the engine creates, or reuses, gets a fresh prefix and
//! nonce; the engine only appends; and the one engine path that rewrites a file
//! in place — writing a global sequence number into an ingested table — is not
//! used by this store. Integrity is the engine's own block and record
//! checksums: the stream does not authenticate, so a flipped byte is caught as
//! corruption by the engine rather than by the cipher.
//!
//! # Which store is which
//!
//! A file named [`MARKER`] beside the engine's files says the store is
//! encrypted, and it is sealed under the key's check subkey, so opening it is
//! how a key is known to be the right one before the engine reads anything.

#![allow(unsafe_code)]

use std::ffi::c_int;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use chacha20::XChaCha20;
use chacha20::cipher::{KeyIvInit as _, StreamCipher as _, StreamCipherSeek as _};
use tessari_kv::Error;
use tessari_vault::AtRestKey;

use crate::error::BACKEND_NAME;

/// The file that marks a store as encrypted.
pub(crate) const MARKER: &str = "ENCRYPTION";

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;

unsafe extern "C" {
    /// An environment encrypting every file under the 32 bytes at `key`, or
    /// null. The adapter copies the key.
    fn tessari_lsm_encrypted_env(key: *const u8) -> *mut librocksdb_sys::rocksdb_env_t;
}

/// The environment an encrypted store runs on.
pub(crate) fn environment(key: &AtRestKey) -> Result<rocksdb::Env, Error> {
    // SAFETY: the pointer is to 32 initialised bytes that outlive the call, and
    // the adapter copies them before returning; it neither keeps nor frees it.
    let raw = unsafe { tessari_lsm_encrypted_env(key.engine().expose().as_ptr()) };
    if raw.is_null() {
        return Err(Error::Unavailable {
            backend: BACKEND_NAME,
            reason: "the engine could not make an encrypted environment".to_owned(),
        });
    }
    // SAFETY: `raw` is a fresh, non-null handle made by `new` in the adapter in
    // the layout the C API defines, and nothing else holds it; ownership passes
    // to the binding, which frees it once through `rocksdb_env_destroy`.
    Ok(unsafe { rocksdb::Env::from_raw(raw) })
}

/// Decide whether `path` opens under `key`, marking a new store as it goes.
///
/// `exists` is whether the engine already has a store there.
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

/// XOR the keystream of `key` and `nonce` at byte `offset` into `data`.
///
/// Answers 0, or 1 when the offset is past the 256 GiB a nonce's keystream
/// covers, or when anything panicked — a panic must not cross into the engine.
///
/// # Safety
///
/// `key` points to 32 readable bytes, `nonce` to 24, and `data` to `length`
/// bytes that nothing else reads or writes for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessari_lsm_keystream(
    key: *const u8,
    nonce: *const u8,
    offset: u64,
    data: *mut u8,
    length: usize,
) -> c_int {
    if length == 0 {
        return 0;
    }
    if key.is_null() || nonce.is_null() || data.is_null() {
        return 1;
    }
    // SAFETY: non-null per the checks above; the caller guarantees 32 and 24
    // readable bytes that outlive the call, which arrays of `u8` need no
    // alignment for. Borrowed rather than copied, so no copy of the key is
    // left on this stack.
    let (key, nonce) = unsafe {
        (
            &*key.cast::<[u8; KEY_BYTES]>(),
            &*nonce.cast::<[u8; NONCE_BYTES]>(),
        )
    };
    // SAFETY: non-null; the caller guarantees `length` bytes exclusively ours
    // for this call, and `u8` needs no alignment.
    let data = unsafe { std::slice::from_raw_parts_mut(data, length) };
    let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The cipher state erases itself on drop (the crate's `zeroize`).
        let mut stream = XChaCha20::new(key.into(), nonce.into());
        stream.try_seek(offset).is_ok() && stream.try_apply_keystream(data).is_ok()
    }));
    c_int::from(!matches!(applied, Ok(true)))
}

/// Fill `length` bytes at `out` from the system's entropy.
///
/// Answers 0, or 1 on refusal or panic.
///
/// # Safety
///
/// `out` points to `length` writable bytes nothing else uses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessari_lsm_entropy(out: *mut u8, length: usize) -> c_int {
    if length == 0 {
        return 0;
    }
    if out.is_null() {
        return 1;
    }
    // SAFETY: non-null; the caller guarantees `length` writable bytes that are
    // ours alone for this call.
    let out = unsafe { std::slice::from_raw_parts_mut(out, length) };
    let filled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        getrandom::fill(out).is_ok()
    }));
    c_int::from(!matches!(filled, Ok(true)))
}
