//! Reading JSON text as a value.
//!
//! There is a JSON **writer** in this workspace already, and no reader, because
//! until now nothing untrusted arrived as JSON: a caller's value reaches a script
//! as a parameter, in this store's own value encoding. A stream message is the
//! first JSON this database has to read rather than write.
//!
//! # Why it is written here rather than pulled in
//!
//! The same reason the writer is hand-written: JSON has six types and this store
//! has seventeen, so the mapping is a decision. A derived reader would make those
//! decisions silently, and two of them matter.
//!
//! **A JSON number is a double in every parser that matters.** So a number with
//! no fraction and no exponent is read as an **integer** and everything else as a
//! float — because a producer sending `1756300000000` means a millisecond
//! timestamp, and a reader that made it `1.7563e12` would round it and land the
//! wrong value with no error anywhere. An integer too large for `i64` is read as
//! a float rather than refused, which loses precision and says so, because
//! refusing the message would stall a partition over a number nobody indexes.
//!
//! **Depth is bounded.** A message is bytes from a broker, so nesting is
//! attacker-controlled, and a recursive-descent reader with no limit is a stack
//! overflow — which aborts the process rather than raising, taking every other
//! consumer and every connection with it.
//!
//! # What it does not do
//!
//! No schema, no type coercion, no date detection. A string stays a string. The
//! mapping clause says which fields land and what they are called, and this
//! reader's whole job is to say what the message contained.

use std::collections::BTreeMap;

use crate::{Number, Value};

/// Why some bytes could not be read as JSON.
///
/// A position rather than a line and column, because the payload is bytes off a
/// wire rather than a file somebody wrote — the useful thing to say is where in
/// the payload, so the operator can look at that byte in the quarantined copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    /// What was wrong.
    pub reason: &'static str,
    /// Which byte of the payload.
    pub at: usize,
}

impl core::fmt::Display for Malformed {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(out, "{} at byte {}", self.reason, self.at)
    }
}

impl std::error::Error for Malformed {}

/// How deep a message may nest.
///
/// Chosen to be far past any real payload and far short of the stack: the point
/// is not to guess a realistic depth, it is that an unbounded one is a crash.
const MAX_DEPTH: usize = 64;

/// Read a whole payload as one JSON value.
///
/// # Errors
///
/// Returns [`Malformed`] when the bytes are not one JSON value, when anything
/// follows it, or when it nests deeper than [`MAX_DEPTH`].
pub fn read(payload: &[u8]) -> Result<Value, Malformed> {
    let mut reader = Reader {
        bytes: payload,
        at: 0,
    };
    reader.space();
    let value = reader.value(0)?;
    reader.space();
    // Trailing content is refused rather than ignored. A payload that is two
    // JSON values is a framing mistake, and reading the first one would apply
    // half a message and report success.
    if reader.at < reader.bytes.len() {
        return Err(reader.wrong("unexpected content after the value"));
    }
    Ok(value)
}

/// One pass over the payload.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    /// The failure at the current position.
    const fn wrong(&self, reason: &'static str) -> Malformed {
        Malformed {
            reason,
            at: self.at,
        }
    }

    /// The byte under the cursor, if there is one.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Step over one byte.
    fn bump(&mut self) {
        self.at = self.at.saturating_add(1);
    }

    /// Step over JSON's four whitespace bytes.
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    /// Consume `word` if it stands here.
    fn word(&mut self, word: &[u8]) -> bool {
        let end = self.at.saturating_add(word.len());
        if self.bytes.get(self.at..end) == Some(word) {
            self.at = end;
            return true;
        }
        false
    }

    /// One value, at `depth` levels of nesting.
    fn value(&mut self, depth: usize) -> Result<Value, Malformed> {
        if depth > MAX_DEPTH {
            return Err(self.wrong("nested too deeply"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.text().map(Value::String),
            Some(b't') if self.word(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.word(b"false") => Ok(Value::Bool(false)),
            // JSON's `null` becomes this store's `null` and not its `none`. The
            // two are different here and JSON has one word, so the reader takes
            // the one that means *present and holding nothing*, which is what a
            // producer that wrote the key meant.
            Some(b'n') if self.word(b"null") => Ok(Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.wrong("not the start of a value")),
            None => Err(self.wrong("the payload ended where a value was expected")),
        }
    }

    /// `{ "a": 1, "b": 2 }`
    fn object(&mut self, depth: usize) -> Result<Value, Malformed> {
        self.bump();
        let mut fields = BTreeMap::new();
        self.space();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Value::Object(fields));
        }
        loop {
            self.space();
            if self.peek() != Some(b'"') {
                return Err(self.wrong("a field name must be a string"));
            }
            let name = self.text()?;
            self.space();
            if self.peek() != Some(b':') {
                return Err(self.wrong("a field name must be followed by `:`"));
            }
            self.bump();
            self.space();
            let held = self.value(depth.saturating_add(1))?;
            // A duplicate key is last-one-wins, which is what every mainstream
            // parser does and therefore what a producer will have been testing
            // against. Refusing would stall a partition over a payload every
            // other consumer of that topic accepts.
            fields.insert(name, held);
            self.space();
            match self.peek() {
                Some(b',') => self.bump(),
                Some(b'}') => {
                    self.bump();
                    return Ok(Value::Object(fields));
                }
                _ => return Err(self.wrong("expected `,` or `}`")),
            }
        }
    }

    /// `[1, 2, 3]`
    fn array(&mut self, depth: usize) -> Result<Value, Malformed> {
        self.bump();
        let mut held = Vec::new();
        self.space();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Value::Array(held));
        }
        loop {
            self.space();
            held.push(self.value(depth.saturating_add(1))?);
            self.space();
            match self.peek() {
                Some(b',') => self.bump(),
                Some(b']') => {
                    self.bump();
                    return Ok(Value::Array(held));
                }
                _ => return Err(self.wrong("expected `,` or `]`")),
            }
        }
    }

    /// A quoted string, with JSON's escapes resolved.
    fn text(&mut self) -> Result<String, Malformed> {
        self.bump();
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.wrong("the payload ended inside a string"));
            };
            self.bump();
            match byte {
                b'"' => return Ok(out),
                b'\\' => self.escape(&mut out)?,
                // A control byte inside a string is invalid JSON. Refused rather
                // than passed through, because passing it through would put a
                // byte in a record that no reader of that record expects.
                0x00..=0x1F => return Err(self.wrong("a control character inside a string")),
                _ => {
                    // Multi-byte UTF-8 is copied through as-is and validated at
                    // the end of the string rather than per byte: the bytes are
                    // already a `&[u8]`, and re-decoding each sequence here would
                    // be a second UTF-8 implementation to get wrong.
                    let start = self.at.saturating_sub(1);
                    let mut end = self.at;
                    while self
                        .bytes
                        .get(end)
                        .is_some_and(|next| !matches!(next, b'"' | b'\\' | 0x00..=0x1F))
                    {
                        end = end.saturating_add(1);
                    }
                    let Some(chunk) = self.bytes.get(start..end) else {
                        return Err(self.wrong("the payload ended inside a string"));
                    };
                    let Ok(text) = core::str::from_utf8(chunk) else {
                        return Err(self.wrong("a string that is not valid UTF-8"));
                    };
                    out.push_str(text);
                    self.at = end;
                }
            }
        }
    }

    /// What follows a backslash inside a string.
    fn escape(&mut self, out: &mut String) -> Result<(), Malformed> {
        let Some(byte) = self.peek() else {
            return Err(self.wrong("the payload ended after a backslash"));
        };
        self.bump();
        out.push(match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode_escape(out),
            _ => return Err(self.wrong("not an escape this reader knows")),
        });
        Ok(())
    }

    /// `\uXXXX`, and the surrogate pair that follows it when there is one.
    fn unicode_escape(&mut self, out: &mut String) -> Result<(), Malformed> {
        let first = self.four_hex()?;
        // A lone high surrogate is refused rather than replaced. Replacing it
        // would put U+FFFD in a record and lose which character was meant, with
        // nothing anywhere saying so.
        let scalar = if (0xD800..0xDC00).contains(&first) {
            if !self.word(b"\\u") {
                return Err(self.wrong("a high surrogate with no pair"));
            }
            let second = self.four_hex()?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err(self.wrong("a high surrogate followed by something else"));
            }
            let high = u32::from(first).saturating_sub(0xD800) << 10_u32;
            let low = u32::from(second).saturating_sub(0xDC00);
            high.saturating_add(low).saturating_add(0x1_0000)
        } else if (0xDC00..0xE000).contains(&first) {
            return Err(self.wrong("a low surrogate with no high one before it"));
        } else {
            u32::from(first)
        };
        let Some(held) = char::from_u32(scalar) else {
            return Err(self.wrong("an escape that is not a character"));
        };
        out.push(held);
        Ok(())
    }

    /// The four hex digits of a `\u` escape.
    fn four_hex(&mut self) -> Result<u16, Malformed> {
        let end = self.at.saturating_add(4);
        let Some(digits) = self.bytes.get(self.at..end) else {
            return Err(self.wrong("the payload ended inside an escape"));
        };
        let mut held: u16 = 0;
        for digit in digits {
            let Some(value) = char::from(*digit).to_digit(16) else {
                return Err(self.wrong("not four hex digits"));
            };
            // Four hex digits cannot exceed `u16`, so neither of these can wrap;
            // they are written saturating because the lint does not know that and
            // a cast that "cannot" overflow is the one that eventually does.
            held = held
                .saturating_mul(16)
                .saturating_add(u16::try_from(value).unwrap_or(u16::MAX));
        }
        self.at = end;
        Ok(held)
    }

    /// A number, read as an integer when it is written as one.
    fn number(&mut self) -> Result<Value, Malformed> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
        }
        let mut whole = true;
        if self.peek() == Some(b'.') {
            whole = false;
            self.bump();
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            whole = false;
            self.bump();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump();
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.bump();
            }
        }
        let Some(written) = self.bytes.get(start..self.at) else {
            return Err(self.wrong("a number that is not there"));
        };
        let Ok(written) = core::str::from_utf8(written) else {
            return Err(self.wrong("a number that is not text"));
        };
        if written.is_empty() || written == "-" {
            return Err(self.wrong("a number with no digits"));
        }
        if whole {
            // The whole reason this reader exists rather than a derived one: a
            // millisecond timestamp read as a double comes back rounded, and
            // nothing anywhere says so.
            if let Ok(held) = written.parse::<i64>() {
                return Ok(Value::Number(Number::Integer(held)));
            }
        }
        written.parse::<f64>().map_or_else(
            |_| Err(self.wrong("a number this reader cannot read")),
            |held| Ok(Value::Number(Number::Float(held))),
        )
    }
}
