//! Reading a message as JSON.
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

use tessari_types::{Number, Value};

/// How deep a message may nest.
///
/// Chosen to be far past any real payload and far short of the stack: the point
/// is not to guess a realistic depth, it is that an unbounded one is a crash.
const MAX_DEPTH: usize = 64;

/// Why a message could not be read.
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

impl std::fmt::Display for Malformed {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{} at byte {}", self.reason, self.at)
    }
}

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
            let Some(value) = (*digit as char).to_digit(16) else {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]

    use super::*;

    fn parsed(text: &str) -> Value {
        read(text.as_bytes()).unwrap_or_else(|failure| panic!("{text} did not read: {failure}"))
    }

    fn refused(text: &str) -> Malformed {
        read(text.as_bytes()).expect_err("this should not have read")
    }

    /// Compare two answers by their written form rather than by `PartialEq`.
    ///
    /// `Value`'s equality equates `Integer(3)`, `Float(3.0)` and
    /// `Decimal("3.0")` deliberately, and the join relies on it. So an
    /// assertion about which KIND this reader answers cannot be written with
    /// `assert_eq!` on the values themselves: it passes against a reader that
    /// answered the other kind, which is the one difference the tests below
    /// exist to catch and the one a caller sees on the wire (Q-76).
    #[track_caller]
    fn same(answered: impl core::fmt::Debug, expected: impl core::fmt::Debug) {
        assert_eq!(format!("{answered:?}"), format!("{expected:?}"));
    }

    #[test]
    fn a_whole_number_stays_whole() {
        // The decision this reader exists for. A millisecond timestamp read as a
        // double comes back rounded, and the record lands with the wrong value
        // and no error anywhere.
        same(
            parsed("1756300000000"),
            Value::Number(Number::Integer(1_756_300_000_000)),
        );
        same(parsed("-7"), Value::Number(Number::Integer(-7)));
        same(parsed("0"), Value::Number(Number::Integer(0)));
    }

    #[test]
    fn a_number_written_with_a_point_or_an_exponent_is_a_float() {
        // Left on `assert_eq!` deliberately. `1.5` has no integer twin and
        // this reader has no decimal path, so there is no kind this assertion
        // could fail to see — converting it would add noise around the two
        // below, which have one (Q-76).
        assert_eq!(parsed("1.5"), Value::Number(Number::Float(1.5)));
        same(parsed("1e3"), Value::Number(Number::Float(1000.0)));
        // Written as a float even though its value is whole: what a producer
        // wrote is what it meant, and `2.0` in a payload is a measurement.
        same(parsed("2.0"), Value::Number(Number::Float(2.0)));
    }

    #[test]
    fn an_integer_too_large_for_i64_degrades_rather_than_stalling_a_partition() {
        // Losing precision on a number nobody indexes is worse than a refusal
        // only in theory; in production a refusal here blocks the partition.
        let Value::Number(Number::Float(held)) = parsed("99999999999999999999999") else {
            panic!("not a float");
        };
        assert!(held > 1e22);
    }

    #[test]
    fn null_is_null_and_not_absent() {
        // This store tells `null` and `none` apart and JSON has one word. A
        // producer that wrote the key meant *present, holding nothing*.
        assert_eq!(parsed("null"), Value::Null);
    }

    #[test]
    fn an_object_reads_into_fields_and_nesting_works() {
        let Value::Object(fields) = parsed(r#"{"a":1,"b":{"c":"x"}}"#) else {
            panic!("not an object");
        };
        same(fields.get("a"), Some(&Value::Number(Number::Integer(1))));
        let Some(Value::Object(inner)) = fields.get("b") else {
            panic!("not nested");
        };
        assert_eq!(inner.get("c"), Some(&Value::from("x")));
    }

    #[test]
    fn the_escapes_resolve_including_a_surrogate_pair() {
        assert_eq!(parsed(r#""a\nb""#), Value::from("a\nb"));
        assert_eq!(parsed(r#""A""#), Value::from("A"));
        // Outside the basic plane, which is where a naive reader emits two
        // replacement characters and nobody notices until an emoji arrives.
        assert_eq!(parsed(r#""😀""#), Value::from("\u{1F600}"));
    }

    #[test]
    fn a_lone_surrogate_is_refused_rather_than_replaced() {
        // Replacing it would put U+FFFD in a record and lose which character was
        // meant, with nothing anywhere saying so.
        assert_eq!(
            refused(r#""\ud83d""#).reason,
            "a high surrogate with no pair"
        );
        assert_eq!(
            refused(r#""\udc00""#).reason,
            "a low surrogate with no high one before it"
        );
    }

    #[test]
    fn multibyte_text_survives_the_round_trip() {
        assert_eq!(parsed(r#""привет 🙂""#), Value::from("привет 🙂"));
    }

    #[test]
    fn a_payload_holding_two_values_is_refused_rather_than_half_read() {
        // A framing mistake. Reading the first value would apply half a message
        // and report success.
        assert_eq!(
            refused("{} {}").reason,
            "unexpected content after the value"
        );
    }

    #[test]
    fn nesting_is_bounded_so_a_message_cannot_overflow_the_stack() {
        // The bytes come off a broker, so the depth is attacker-controlled. An
        // overflow aborts the process, taking every other consumer and every
        // open connection with it — which is not a failure any `on_failure`
        // policy could have caught.
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
        assert_eq!(refused(&deep).reason, "nested too deeply");
    }

    #[test]
    fn the_ordinary_malformed_payloads_all_say_where() {
        for (text, reason) in [
            ("", "the payload ended where a value was expected"),
            ("{", "a field name must be a string"),
            (r#"{"a""#, "a field name must be followed by `:`"),
            (r#"{"a":1"#, "expected `,` or `}`"),
            ("[1", "expected `,` or `]`"),
            (r#""unterminated"#, "the payload ended inside a string"),
            ("tru", "not the start of a value"),
            ("-", "a number with no digits"),
        ] {
            let failure = refused(text);
            assert_eq!(failure.reason, reason, "for {text:?}");
        }
    }

    #[test]
    fn an_empty_object_and_an_empty_array_read() {
        assert_eq!(parsed("{}"), Value::Object(BTreeMap::new()));
        assert_eq!(parsed("[]"), Value::Array(Vec::new()));
        assert_eq!(parsed("  { }  "), Value::Object(BTreeMap::new()));
    }

    #[test]
    fn invalid_utf8_in_a_string_is_refused() {
        // A byte sequence that is not text cannot become a string field, and
        // passing it through would put bytes in a record no reader expects.
        let payload = [b'"', 0xFF, 0xFE, b'"'];
        assert_eq!(
            read(&payload).expect_err("read invalid utf-8").reason,
            "a string that is not valid UTF-8"
        );
    }
}
