//! A sealed backup: any backup's bytes, encrypted and authenticated as a stream.
//!
//! # The construction
//!
//! ```text
//! head   "TESSARISEALED" <version:u8> <prefix:7>
//! chunk  ChaCha20-Poly1305(backup subkey, nonce = prefix ‖ counter:u32 ‖ last:u8,
//!                          aad = head, 64 KiB of plaintext)    -- every chunk but the last
//! last   the same with last = 1 and 0..=64 KiB of plaintext
//! ```
//!
//! This is the STREAM construction: each chunk is an AEAD message whose nonce
//! carries its position and whether it ends the stream. A chunk moved, dropped
//! or repeated fails at its own position; a file cut on a chunk boundary ends
//! on a chunk not marked last and fails there; bytes appended after the last
//! chunk fail too. The head is authenticated with every chunk, so it cannot be
//! swapped either. The 7-byte prefix is random per file, which is what keeps
//! two backups under one key from ever sharing a nonce.
//!
//! A reader hands out plaintext only from chunks that authenticated. A file that
//! fails part-way has already given its earlier chunks to the caller, which is
//! why every consumer of a backup here verifies the whole of it before applying
//! any — the same rule an unsealed backup already lives under.

use std::io::{self, Read, Write};

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};

use super::AtRestKey;
use crate::error::{Error, Result};
use crate::secret::SecretBytes;

/// What a sealed backup starts with.
pub const SEALED_MAGIC: &[u8; 13] = b"TESSARISEALED";
const VERSION: u8 = 1;
const PREFIX_BYTES: usize = 7;
/// Where the version byte sits, and the random prefix after it.
const VERSION_AT: usize = SEALED_MAGIC.len();
const PREFIX_AT: usize = VERSION_AT + 1;
const HEAD_BYTES: usize = PREFIX_AT + PREFIX_BYTES;
/// Where the counter and the last-chunk flag sit in a nonce.
const COUNTER_AT: usize = PREFIX_BYTES;
const LAST_AT: usize = COUNTER_AT + 4;
/// Plaintext per chunk.
const CHUNK_BYTES: usize = 64 * 1024;
const TAG_BYTES: usize = 16;
/// A whole chunk as written.
const SEALED_CHUNK_BYTES: usize = CHUNK_BYTES + TAG_BYTES;

/// The cipher and the head every chunk is bound to.
struct Chunks {
    cipher: ChaCha20Poly1305,
    head: [u8; HEAD_BYTES],
    counter: u32,
}

impl Chunks {
    fn new(key: &SecretBytes, head: [u8; HEAD_BYTES]) -> Result<Self> {
        Ok(Self {
            cipher: ChaCha20Poly1305::new_from_slice(key.expose())
                .map_err(|_| Error::BackupDoesNotOpen)?,
            head,
            counter: 0,
        })
    }

    fn nonce(&self, last: bool) -> Nonce {
        let mut nonce = [0_u8; 12];
        nonce[..COUNTER_AT].copy_from_slice(&self.head[PREFIX_AT..]);
        nonce[COUNTER_AT..LAST_AT].copy_from_slice(&self.counter.to_be_bytes());
        nonce[LAST_AT] = u8::from(last);
        Nonce::from(nonce)
    }

    /// Move past a chunk; a stream of four billion chunks is 256 TiB, and one
    /// longer is refused rather than allowed to repeat a nonce.
    fn advance(&mut self) -> io::Result<()> {
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("a sealed backup cannot be this long"))?;
        Ok(())
    }

    fn seal(&mut self, plain: &[u8], last: bool) -> io::Result<Vec<u8>> {
        let sealed = self
            .cipher
            .encrypt(
                &self.nonce(last),
                Payload {
                    msg: plain,
                    aad: &self.head,
                },
            )
            .map_err(|_| io::Error::other("a backup chunk could not be sealed"))?;
        self.advance()?;
        Ok(sealed)
    }

    fn open(&mut self, sealed: &[u8], last: bool) -> io::Result<Vec<u8>> {
        let plain = self
            .cipher
            .decrypt(
                &self.nonce(last),
                Payload {
                    msg: sealed,
                    aad: &self.head,
                },
            )
            .map_err(|_| refused(Error::BackupDoesNotOpen))?;
        self.advance()?;
        Ok(plain)
    }
}

fn refused(error: Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// A writer that seals what passes through it.
///
/// [`Sealing::finish`] writes the last chunk. A sealing writer dropped without
/// it leaves a file that never opens, which is the right failure for a backup
/// that was not finished.
pub struct Sealing<W: Write> {
    out: W,
    chunks: Chunks,
    pending: Vec<u8>,
}

impl<W: Write> Sealing<W> {
    pub(super) fn new(key: &SecretBytes, mut out: W) -> io::Result<Self> {
        let mut head = [0_u8; HEAD_BYTES];
        head[..VERSION_AT].copy_from_slice(SEALED_MAGIC);
        head[VERSION_AT] = VERSION;
        getrandom::fill(&mut head[PREFIX_AT..]).map_err(|_| io::Error::other(Error::Entropy))?;
        let chunks = Chunks::new(key, head).map_err(io::Error::other)?;
        out.write_all(&head)?;
        Ok(Self {
            out,
            chunks,
            pending: Vec::with_capacity(CHUNK_BYTES),
        })
    }

    /// Write the last chunk and hand back the writer.
    ///
    /// # Errors
    ///
    /// The writer's own failure.
    pub fn finish(mut self) -> io::Result<W> {
        let last = self.chunks.seal(&self.pending, true)?;
        self.out.write_all(&last)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Sealing<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // A full chunk is sealed only when more follows it, because the last
        // chunk is marked as last and a writer does not know which one is last
        // until it is finished.
        if self.pending.len() == CHUNK_BYTES && !bytes.is_empty() {
            let sealed = self.chunks.seal(&self.pending, false)?;
            self.out.write_all(&sealed)?;
            self.pending.clear();
        }
        let room = CHUNK_BYTES.saturating_sub(self.pending.len());
        let taken = bytes.len().min(room);
        self.pending.extend_from_slice(&bytes[..taken]);
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// A reader that opens a sealed backup.
pub struct Opening<R: Read> {
    input: R,
    chunks: Chunks,
    /// Sealed bytes read ahead: one chunk and one byte, which is how a chunk
    /// is known not to be the last.
    ahead: Vec<u8>,
    plain: Vec<u8>,
    given: usize,
    ended: bool,
}

impl<R: Read> Opening<R> {
    fn new(key: &SecretBytes, head: [u8; HEAD_BYTES], input: R) -> Result<Self> {
        Ok(Self {
            input,
            chunks: Chunks::new(key, head)?,
            ahead: Vec::with_capacity(SEALED_CHUNK_BYTES + 1),
            plain: Vec::new(),
            given: 0,
            ended: false,
        })
    }

    /// Open the next chunk into `plain`.
    fn next_chunk(&mut self) -> io::Result<()> {
        fill(&mut self.input, &mut self.ahead, SEALED_CHUNK_BYTES + 1)?;
        let last = self.ahead.len() <= SEALED_CHUNK_BYTES;
        let length = self.ahead.len().min(SEALED_CHUNK_BYTES);
        if length < TAG_BYTES {
            return Err(refused(Error::BackupDoesNotOpen));
        }
        self.plain = self.chunks.open(&self.ahead[..length], last)?;
        self.ahead.drain(..length);
        self.given = 0;
        self.ended = last;
        Ok(())
    }
}

impl<R: Read> Read for Opening<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.given == self.plain.len() {
            if self.ended {
                return Ok(0);
            }
            self.next_chunk()?;
        }
        let taken = out.len().min(self.plain.len().saturating_sub(self.given));
        let end = self.given.saturating_add(taken);
        out[..taken].copy_from_slice(&self.plain[self.given..end]);
        self.given = end;
        Ok(taken)
    }
}

/// Read into `buffer` until it holds `want` bytes or the input ends.
fn fill(input: &mut impl Read, buffer: &mut Vec<u8>, want: usize) -> io::Result<()> {
    while buffer.len() < want {
        let start = buffer.len();
        buffer.resize(want, 0);
        match input.read(&mut buffer[start..]) {
            Ok(0) => {
                buffer.truncate(start);
                return Ok(());
            }
            Ok(read) => buffer.truncate(start.saturating_add(read)),
            Err(failure) if failure.kind() == io::ErrorKind::Interrupted => buffer.truncate(start),
            Err(failure) => {
                buffer.truncate(start);
                return Err(failure);
            }
        }
    }
    Ok(())
}

/// A backup as read: opened if it was sealed, as it is if it was not.
pub enum Reading<R: Read> {
    /// A plain backup; the bytes already looked at come first.
    Plain(io::Chain<io::Cursor<Vec<u8>>, R>),
    /// A sealed backup, opening as it is read.
    Opened(Box<Opening<R>>),
}

impl<R: Read> Read for Reading<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(plain) => plain.read(out),
            Self::Opened(opening) => opening.read(out),
        }
    }
}

/// Read a backup that may be sealed.
///
/// A sealed one opens under `key`; with no key it is refused, naming why. A
/// plain one passes through whether or not there is a key — a plain backup
/// restores into an encrypted store.
///
/// # Errors
///
/// [`Error::BackupSealed`] for a sealed backup and no key,
/// [`Error::UnknownVersion`] for a sealed format this build does not know, and
/// the input's own failure.
pub fn reading<R: Read>(key: Option<&AtRestKey>, mut input: R) -> io::Result<Reading<R>> {
    let mut start = Vec::with_capacity(HEAD_BYTES);
    fill(&mut input, &mut start, HEAD_BYTES)?;
    let Ok(head) = <[u8; HEAD_BYTES]>::try_from(start.as_slice()) else {
        return Ok(Reading::Plain(io::Cursor::new(start).chain(input)));
    };
    if !head.starts_with(SEALED_MAGIC) {
        return Ok(Reading::Plain(io::Cursor::new(start).chain(input)));
    }
    let key = key.ok_or_else(|| refused(Error::BackupSealed))?;
    let version = head[VERSION_AT];
    if version != VERSION {
        return Err(refused(Error::UnknownVersion(version)));
    }
    let opening = Opening::new(key.backups(), head, input).map_err(refused)?;
    Ok(Reading::Opened(Box::new(opening)))
}
