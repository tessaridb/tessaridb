//! Commands read from a RESP client: an array of bulk strings, or one inline
//! line typed into a terminal.
//!
//! Bytes arrive in pieces and a command may span many of them, so the decoder
//! keeps what it has read of a half-arrived command and never reads the same
//! bytes twice: a million small arguments arriving slowly cost a million
//! reads, not a million squared. Every length is checked against its ceiling
//! before anything is reserved for it.

use tessari_constants::{
    RESP_INLINE_MAX_BYTES, RESP_MAX_ARGUMENTS, RESP_MAX_BULK_BYTES, RESP_MAX_COMMAND_BYTES,
};

use crate::error::{Error, Result};

/// One command: its name and its arguments, the bytes the client sent.
pub type Command = Vec<Vec<u8>>;

/// The longest length line read before its `\r\n`: a sign, nineteen digits.
const LENGTH_LINE: usize = 21;

/// Bytes arrived and not yet read as commands.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    /// How much of `buffer` has been read already.
    read: usize,
    /// An array whose header has been read and whose arguments have not all
    /// arrived.
    partial: Option<Partial>,
}

#[derive(Debug)]
struct Partial {
    remaining: usize,
    arguments: Command,
    held: usize,
}

impl Decoder {
    /// An empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take bytes the client sent.
    pub fn extend(&mut self, bytes: &[u8]) {
        if self.read > 0 && self.read >= self.buffer.len() / 2 {
            self.buffer.drain(..self.read);
            self.read = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// Whether bytes are held that are not a whole command yet.
    #[must_use]
    pub fn holds_a_part(&self) -> bool {
        self.partial.is_some() || self.read < self.buffer.len()
    }

    /// The next whole command, or `None` until more bytes arrive.
    ///
    /// # Errors
    ///
    /// Bytes that are not a command, or a length past its ceiling; the
    /// connection cannot be read any further.
    pub fn command(&mut self) -> Result<Option<Command>> {
        loop {
            if let Some(mut partial) = self.partial.take() {
                while partial.remaining > 0 {
                    let Some(argument) = self.bulk(partial.held)? else {
                        self.partial = Some(partial);
                        return Ok(None);
                    };
                    partial.held = partial.held.saturating_add(argument.len());
                    partial.remaining = partial.remaining.saturating_sub(1);
                    partial.arguments.push(argument);
                }
                return Ok(Some(partial.arguments));
            }
            let Some(&first) = self.buffer.get(self.read) else {
                return Ok(None);
            };
            if first != b'*' {
                match self.inline()? {
                    // A blank line, which a terminal sends on an empty enter.
                    Some(words) if words.is_empty() => continue,
                    read => return Ok(read),
                }
            }
            let Some((count, used)) = number(&self.buffer[self.read..], "array length")? else {
                return Ok(None);
            };
            self.read = self.read.saturating_add(used);
            // An empty or null array asks for nothing, and is passed over.
            let Ok(count) = usize::try_from(count) else {
                continue;
            };
            if count == 0 {
                continue;
            }
            if count > RESP_MAX_ARGUMENTS {
                return Err(too_large("the argument count", count, RESP_MAX_ARGUMENTS));
            }
            self.partial = Some(Partial {
                remaining: count,
                arguments: Vec::with_capacity(count.min(64)),
                held: 0,
            });
        }
    }

    /// One bulk string, read whole or not at all.
    fn bulk(&mut self, held: usize) -> Result<Option<Vec<u8>>> {
        let rest = &self.buffer[self.read..];
        let Some(&first) = rest.first() else {
            return Ok(None);
        };
        if first != b'$' {
            return Err(Error::Malformed {
                field: "an argument",
                why: "an array of commands holds bulk strings",
            });
        }
        let Some((length, used)) = number(rest, "bulk length")? else {
            return Ok(None);
        };
        let length = usize::try_from(length).map_err(|_| Error::Malformed {
            field: "bulk length",
            why: "an argument cannot be null",
        })?;
        if length > RESP_MAX_BULK_BYTES {
            return Err(too_large("a bulk length", length, RESP_MAX_BULK_BYTES));
        }
        let together = held.saturating_add(length);
        if together > RESP_MAX_COMMAND_BYTES {
            return Err(too_large(
                "a command's arguments",
                together,
                RESP_MAX_COMMAND_BYTES,
            ));
        }
        let end = used.saturating_add(length);
        let Some(after) = rest.get(end..end.saturating_add(2)) else {
            return Ok(None);
        };
        if after != b"\r\n" {
            return Err(Error::Malformed {
                field: "a bulk string",
                why: "it does not end where its length says",
            });
        }
        let argument = rest[used..end].to_vec();
        self.read = self.read.saturating_add(end).saturating_add(2);
        Ok(Some(argument))
    }

    /// One inline command: words on a line, as a terminal sends them; a blank
    /// line reads as no words.
    fn inline(&mut self) -> Result<Option<Command>> {
        let rest = &self.buffer[self.read..];
        let Some(end) = rest.iter().position(|byte| *byte == b'\n') else {
            if rest.len() > RESP_INLINE_MAX_BYTES {
                return Err(too_large(
                    "an inline command",
                    rest.len(),
                    RESP_INLINE_MAX_BYTES,
                ));
            }
            return Ok(None);
        };
        if end > RESP_INLINE_MAX_BYTES {
            return Err(too_large("an inline command", end, RESP_INLINE_MAX_BYTES));
        }
        let line = rest[..end].strip_suffix(b"\r").unwrap_or(&rest[..end]);
        let words: Command = line
            .split(u8::is_ascii_whitespace)
            .filter(|word| !word.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        self.read = self.read.saturating_add(end).saturating_add(1);
        Ok(Some(words))
    }
}

/// A `*`/`$` length line at the start of `bytes`: the number and how many
/// bytes the line took, or `None` until its `\r\n` arrives.
fn number(bytes: &[u8], field: &'static str) -> Result<Option<(i64, usize)>> {
    let Some(end) = bytes
        .iter()
        .take(LENGTH_LINE.saturating_add(2))
        .position(|byte| *byte == b'\r')
    else {
        if bytes.len() > LENGTH_LINE {
            return Err(Error::Malformed {
                field,
                why: "it is not a number",
            });
        }
        return Ok(None);
    };
    let Some(&after) = bytes.get(end.saturating_add(1)) else {
        return Ok(None);
    };
    if after != b'\n' {
        return Err(Error::Malformed {
            field,
            why: "its line does not end in \\r\\n",
        });
    }
    let digits = std::str::from_utf8(&bytes[1..end]).map_err(|_| Error::Malformed {
        field,
        why: "it is not a number",
    })?;
    let value = digits.parse::<i64>().map_err(|_| Error::Malformed {
        field,
        why: "it is not a number",
    })?;
    Ok(Some((value, end.saturating_add(2))))
}

fn too_large(field: &'static str, found: usize, ceiling: usize) -> Error {
    Error::TooLarge {
        field,
        found: u64::try_from(found).unwrap_or(u64::MAX),
        ceiling: u64::try_from(ceiling).unwrap_or(u64::MAX),
    }
}

#[cfg(test)]
mod tests;
