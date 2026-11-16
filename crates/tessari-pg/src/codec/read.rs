//! Decoding what a frontend sends: the first packet, then the messages of a
//! running session. Every decoder takes the payload of one frame — the length
//! was read and checked by the caller.

use crate::Result;
use crate::codec::{CANCEL_REQUEST, SSL_REQUEST};
use crate::error::Error;

/// The first packet on a connection.
#[derive(Debug)]
pub enum First {
    /// An SSL negotiation is asked for; the caller answers `S` or `N`.
    Ssl,
    /// A cancel request for `pid`/`key`; the caller closes without answering.
    Cancel {
        /// The process id the request names.
        pid: u32,
        /// The secret key the request must carry.
        key: u32,
    },
    /// The startup packet: the protocol version and the named pairs — user,
    /// database, and whatever else the client was configured with.
    Startup {
        /// The offered protocol version, major in the high half.
        version: u32,
        /// The key/value pairs, in the order sent.
        pairs: Vec<(String, String)>,
    },
}

/// One message of a running session.
#[derive(Debug)]
pub enum Frontend {
    /// One query string, which the protocol says may hold several statements.
    Query(String),
    /// The password answering a cleartext challenge.
    Password(String),
    /// The end of an extended-protocol batch: the surface answers with what it
    /// has and a fresh readiness, honestly refusing the batch's contents.
    Sync,
    /// Deliver what is buffered; the surface holds nothing back, so this is a
    /// no-op.
    Flush,
    /// The client is going away; the connection ends cleanly.
    Terminate,
    /// A Parse of one statement: a name, the query text, and the parameter
    /// type ids it declares (zero where the client leaves the choice open).
    Parse {
        /// The statement's name; empty is the unnamed statement.
        name: String,
        /// The query text, with `$1..$n` where values arrive at Bind.
        query: String,
        /// The declared parameter type ids.
        types: Vec<u32>,
    },
    /// A Bind of a portal over a statement: the formats and values the
    /// parameters arrive in, and the formats the results are asked for.
    Bind {
        /// The portal's name; empty is the unnamed portal.
        portal: String,
        /// The statement the portal runs.
        statement: String,
        /// The parameter formats, one per value when more than one is named.
        formats: Vec<u16>,
        /// The parameter values; `None` is the dialect's NULL.
        values: Vec<Option<Vec<u8>>>,
        /// The result formats the client asks for.
        results: Vec<u16>,
    },
    /// A Describe of a statement (`b'S'`) or a portal (`b'P'`).
    Describe {
        /// Which kind of thing is described.
        kind: u8,
        /// Its name.
        name: String,
    },
    /// An Execute of a portal, asking for at most `rows` rows.
    Execute {
        /// The portal's name.
        portal: String,
        /// The row ceiling; zero asks for everything.
        rows: u32,
    },
    /// A Close of a statement or a portal, named the way Describe names them.
    Close {
        /// Which kind of thing is closed.
        kind: u8,
        /// Its name.
        name: String,
    },
    /// A message this version of the surface does not speak. The kind byte is
    /// kept so the refusal can name what arrived.
    Unspoken(u8),
}

/// Decode a first packet's payload. `payload` is everything after the length
/// word; a first packet carries at least the request code or the version.
pub(crate) fn first(payload: &[u8]) -> Result<First> {
    let mut at = Cursor::new(payload);
    let code = at.u32()?;
    match code {
        SSL_REQUEST => {
            at.done()?;
            Ok(First::Ssl)
        }
        CANCEL_REQUEST => {
            let pid = at.u32()?;
            let key = at.u32()?;
            at.done()?;
            Ok(First::Cancel { pid, key })
        }
        version => {
            let mut pairs = Vec::new();
            while !at.exhausted() {
                let key = at.text()?;
                if key.is_empty() {
                    break;
                }
                let value = at.text()?;
                pairs.push((key, value));
            }
            at.done()?;
            Ok(First::Startup { version, pairs })
        }
    }
}

/// Decode one message of a running session from its kind byte and payload.
pub(crate) fn message(kind: u8, payload: &[u8]) -> Result<Frontend> {
    let mut at = Cursor::new(payload);
    let decoded = match kind {
        b'Q' => {
            let query = at.text()?;
            at.done()?;
            Frontend::Query(query)
        }
        b'p' => {
            let password = at.text()?;
            at.done()?;
            Frontend::Password(password)
        }
        b'S' => {
            at.done()?;
            Frontend::Sync
        }
        b'H' => {
            at.done()?;
            Frontend::Flush
        }
        b'X' => {
            at.done()?;
            Frontend::Terminate
        }
        b'P' => {
            let name = at.text()?;
            let query = at.text()?;
            let count = usize::from(at.u16()?);
            let mut types = Vec::with_capacity(count);
            for _ in 0..count {
                types.push(at.u32()?);
            }
            at.done()?;
            Frontend::Parse { name, query, types }
        }
        b'B' => {
            let portal = at.text()?;
            let statement = at.text()?;
            let formats = at.formats()?;
            let count = usize::from(at.u16()?);
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                let len = at.i32()?;
                if len < 0 {
                    values.push(None);
                } else {
                    let len = usize::try_from(len)
                        .map_err(|_| Error::Malformed("a parameter does not fit"))?;
                    values.push(Some(at.take(len)?.to_vec()));
                }
            }
            let results = at.formats()?;
            at.done()?;
            Frontend::Bind {
                portal,
                statement,
                formats,
                values,
                results,
            }
        }
        b'D' => {
            let kind = at.byte()?;
            let name = at.text()?;
            at.done()?;
            Frontend::Describe { kind, name }
        }
        b'E' => {
            let portal = at.text()?;
            let rows = at.u32()?;
            at.done()?;
            Frontend::Execute { portal, rows }
        }
        b'C' => {
            let kind = at.byte()?;
            let name = at.text()?;
            at.done()?;
            Frontend::Close { kind, name }
        }
        // An unspoken message's body is not parsed, so nothing of it is owed:
        // the refusal names the kind and the conversation moves on.
        other => Frontend::Unspoken(other),
    };
    Ok(decoded)
}

/// A read cursor over one frame's payload, refusing truncation and trailing
/// bytes with the same word.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn exhausted(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(Error::Malformed("length overflow"))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(Error::Malformed("truncated frame"))?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        let word: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| Error::Malformed("a word is four bytes"))?;
        Ok(u32::from_be_bytes(word))
    }

    fn u16(&mut self) -> Result<u16> {
        let word: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| Error::Malformed("a short word is two bytes"))?;
        Ok(u16::from_be_bytes(word))
    }

    fn i32(&mut self) -> Result<i32> {
        let word: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| Error::Malformed("a word is four bytes"))?;
        Ok(i32::from_be_bytes(word))
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = self
            .take(1)?
            .first()
            .copied()
            .ok_or(Error::Malformed("a byte is one byte"))?;
        Ok(byte)
    }

    /// The format words of a Bind: a count, then that many u16s.
    fn formats(&mut self) -> Result<Vec<u16>> {
        let count = usize::from(self.u16()?);
        let mut formats = Vec::with_capacity(count);
        for _ in 0..count {
            formats.push(self.u16()?);
        }
        Ok(formats)
    }

    /// One null-terminated string; the terminator is part of the frame.
    fn text(&mut self) -> Result<String> {
        let rest = &self.bytes[self.at..];
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(Error::Malformed("unterminated string"))?;
        let word = &rest[..end];
        self.at = self.at.saturating_add(end).saturating_add(1);
        String::from_utf8(word.to_vec()).map_err(|_| Error::Malformed("string is not utf-8"))
    }

    /// The payload must end exactly where the last field ended.
    fn done(&self) -> Result<()> {
        if self.exhausted() {
            Ok(())
        } else {
            Err(Error::Malformed("trailing bytes after the message"))
        }
    }
}
