//! Order-preserving component encodings.
//!
//! The substrate compares keys as unsigned bytes, lexicographically. Every
//! encoder here therefore has one job beyond round-tripping: **byte order must
//! equal logical order**. An encoder that round-trips correctly and sorts wrongly
//! passes every test that only checks round-tripping, and then returns the wrong
//! set from every range scan without raising anything.
//!
//! The rules, and why each exists:
//!
//! - Integers are fixed-width big-endian. Text formatting puts `"10"` before
//!   `"9"`; little-endian puts the least significant byte first.
//! - Signed integers additionally flip the sign bit, which moves negatives below
//!   positives instead of above them.
//! - Variable-length components are escaped and terminated, never
//!   length-prefixed. A length prefix sorts `"b"` before `"aa"`.
//! - Descending order is the bitwise complement.
//!
//! `docs/key-grammar.md` §4 is the normative statement; this module implements
//! it.

use crate::error::{Error, Result};
use crate::kind::KeyKind;

/// Byte that introduces an escape sequence and terminates a component.
const ESCAPE: u8 = 0x00;
/// Second byte of a component terminator: `0x00 0x01`.
const TERMINATOR: u8 = 0x01;
/// Second byte of an escaped zero: `0x00 0xFF`.
const ESCAPED_ZERO: u8 = 0xFF;

/// Builds a key by appending order-preserving components.
#[derive(Debug, Default, Clone)]
pub struct KeyWriter {
    buffer: Vec<u8>,
}

impl KeyWriter {
    /// An empty writer.
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// An empty writer with room for `capacity` bytes.
    ///
    /// Key lengths are known within a few bytes at every call site here, and
    /// keys are built per operation, so the reservation is worth making.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
        }
    }

    /// Append a single byte verbatim — a kind tag or a discriminant.
    pub fn put_u8(&mut self, value: u8) -> &mut Self {
        self.buffer.push(value);
        self
    }

    /// Append a `u32` in ascending order.
    pub fn put_u32(&mut self, value: u32) -> &mut Self {
        self.buffer.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Append a `u64` in ascending order.
    pub fn put_u64(&mut self, value: u64) -> &mut Self {
        self.buffer.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Append an `i64` in ascending order, negatives first.
    ///
    /// Two's-complement big-endian puts negatives *above* positives, because
    /// their leading bit is set. Flipping that bit maps `i64::MIN` to all-zero
    /// bytes and `i64::MAX` to all-ones, restoring numeric order.
    pub fn put_i64(&mut self, value: i64) -> &mut Self {
        let mut bytes = value.to_be_bytes();
        bytes[0] ^= 0x80;
        self.buffer.extend_from_slice(&bytes);
        self
    }

    /// Append a `u64` in *descending* order — largest value sorts first.
    ///
    /// Used for the MVCC version suffix, so the newest version of a record is
    /// the first entry under its prefix.
    pub fn put_u64_descending(&mut self, value: u64) -> &mut Self {
        self.buffer.extend_from_slice(&(!value).to_be_bytes());
        self
    }

    /// Append a fixed-width byte block verbatim.
    ///
    /// Only valid when the width is implied by what precedes it; otherwise the
    /// block is indistinguishable from what follows.
    pub fn put_fixed(&mut self, value: &[u8]) -> &mut Self {
        self.buffer.extend_from_slice(value);
        self
    }

    /// Append a variable-length component, escaped and terminated.
    ///
    /// The terminator `0x00 0x01` is smaller than an escaped zero `0x00 0xFF`,
    /// which is what makes a shorter component sort before a longer one that
    /// extends it.
    pub fn put_variable(&mut self, value: &[u8]) -> &mut Self {
        self.buffer.reserve(value.len().saturating_add(2));
        for &byte in value {
            if byte == ESCAPE {
                self.buffer.push(ESCAPE);
                self.buffer.push(ESCAPED_ZERO);
            } else {
                self.buffer.push(byte);
            }
        }
        self.buffer.push(ESCAPE);
        self.buffer.push(TERMINATOR);
        self
    }

    /// The encoded bytes.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buffer
    }
}

/// Reads components back out of a key.
///
/// The reader carries the kind it is decoding so that every error names the
/// subsystem responsible for the bytes.
#[derive(Debug)]
pub struct KeyReader<'a> {
    kind: KeyKind,
    input: &'a [u8],
    position: usize,
}

impl<'a> KeyReader<'a> {
    /// Start reading `input` as a key of `kind`.
    #[must_use]
    pub const fn new(kind: KeyKind, input: &'a [u8]) -> Self {
        Self {
            kind,
            input,
            position: 0,
        }
    }

    /// How many bytes are still unread.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
    }

    /// The offset the next read starts at, for error reporting.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    /// The kind being decoded, so callers can attribute their own errors to it.
    #[must_use]
    pub const fn kind(&self) -> KeyKind {
        self.kind
    }

    /// Read the leading kind tag and check it against the expected kind.
    pub fn expect_kind(&mut self) -> Result<()> {
        let tag = self.take_u8()?;
        if tag == self.kind.tag() {
            return Ok(());
        }
        Err(Error::UnexpectedKind {
            expected: self.kind,
            found: tag,
            found_name: KeyKind::from_tag(tag).map_or("unassigned", KeyKind::name),
        })
    }

    /// Read one byte.
    pub fn take_u8(&mut self) -> Result<u8> {
        let bytes = self.take_fixed::<1>()?;
        Ok(bytes[0])
    }

    /// The bytes consumed since `start`.
    ///
    /// Lets a caller keep a component's bytes verbatim when the component's own
    /// encoding cannot be reversed — an index field, whose numbers are
    /// normalised on the way in.
    #[must_use]
    pub fn consumed_since(&self, start: usize) -> &'a [u8] {
        self.input.get(start..self.position).unwrap_or_default()
    }

    /// Look at the next byte without consuming it.
    ///
    /// A container ends with a terminator that is *below* every element tag, so
    /// deciding whether the next thing is another element or the end has to
    /// happen before the byte is taken.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] when nothing is left.
    pub fn peek(&self) -> Result<u8> {
        self.input
            .get(self.position)
            .copied()
            .ok_or(Error::Truncated {
                kind: self.kind,
                offset: self.position,
                needed: 1,
                available: 0,
            })
    }

    /// Read a `u32` written by [`KeyWriter::put_u32`].
    pub fn take_u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take_fixed::<4>()?))
    }

    /// Read a `u64` written by [`KeyWriter::put_u64`].
    pub fn take_u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take_fixed::<8>()?))
    }

    /// Read an `i64` written by [`KeyWriter::put_i64`].
    pub fn take_i64(&mut self) -> Result<i64> {
        let mut bytes = self.take_fixed::<8>()?;
        bytes[0] ^= 0x80;
        Ok(i64::from_be_bytes(bytes))
    }

    /// Read a `u64` written by [`KeyWriter::put_u64_descending`].
    pub fn take_u64_descending(&mut self) -> Result<u64> {
        Ok(!u64::from_be_bytes(self.take_fixed::<8>()?))
    }

    /// Read exactly `N` bytes.
    pub fn take_fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.position.saturating_add(N);
        let slice = self.input.get(self.position..end).ok_or(Error::Truncated {
            kind: self.kind,
            offset: self.position,
            needed: N,
            available: self.remaining(),
        })?;
        let mut out = [0_u8; N];
        out.copy_from_slice(slice);
        self.position = end;
        Ok(out)
    }

    /// Read exactly `len` bytes, where the length came from the data itself.
    ///
    /// Distinct from [`Self::take_fixed`], whose length is a compile-time
    /// constant. A length read out of the input is not trusted: a truncated or
    /// tampered payload can name more bytes than are there, and this is the
    /// bounds check that turns that into a typed error instead of a panic.
    pub fn take_exact(&mut self, len: usize) -> Result<Vec<u8>> {
        let end = self.position.saturating_add(len);
        let slice = self.input.get(self.position..end).ok_or(Error::Truncated {
            kind: self.kind,
            offset: self.position,
            needed: len,
            available: self.remaining(),
        })?;
        let out = slice.to_vec();
        self.position = end;
        Ok(out)
    }

    /// Read a component written by [`KeyWriter::put_variable`].
    pub fn take_variable(&mut self) -> Result<Vec<u8>> {
        let start = self.position;
        let mut out = Vec::new();
        loop {
            let byte = self.take_u8().map_err(|_| Error::UnterminatedComponent {
                kind: self.kind,
                offset: start,
            })?;
            if byte != ESCAPE {
                out.push(byte);
                continue;
            }
            let escape_offset = self.position.saturating_sub(1);
            let next = self.take_u8().map_err(|_| Error::UnterminatedComponent {
                kind: self.kind,
                offset: start,
            })?;
            match next {
                TERMINATOR => return Ok(out),
                ESCAPED_ZERO => out.push(ESCAPE),
                found => {
                    return Err(Error::InvalidEscape {
                        kind: self.kind,
                        offset: escape_offset,
                        found,
                    });
                }
            }
        }
    }

    /// Assert the key ended exactly where the caller expected it to.
    ///
    /// Trailing bytes mean the key is not what it claims to be. Ignoring them
    /// would decode a longer key as a shorter one and address the wrong object.
    pub fn finish(self) -> Result<()> {
        let extra = self.remaining();
        if extra == 0 {
            return Ok(());
        }
        Err(Error::TrailingBytes {
            kind: self.kind,
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome; the
    // lints below target production paths.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    fn variable(value: &[u8]) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        writer.put_variable(value);
        writer.finish()
    }

    #[test]
    fn variable_components_match_the_documented_vectors() {
        // These are the worked cases in `docs/key-grammar.md` §4.3.
        assert_eq!(variable(b"a"), vec![0x61, 0x00, 0x01]);
        assert_eq!(variable(b"ab"), vec![0x61, 0x62, 0x00, 0x01]);
        assert_eq!(variable(b"a\x00"), vec![0x61, 0x00, 0xFF, 0x00, 0x01]);
        assert_eq!(variable(b""), vec![0x00, 0x01]);
    }

    #[test]
    fn a_shorter_component_sorts_before_a_longer_one_extending_it() {
        assert!(variable(b"a") < variable(b"ab"));
        assert!(variable(b"a") < variable(b"a\x00"));
        assert!(variable(b"") < variable(b"a"));
    }

    #[test]
    fn signed_integers_put_negatives_first() {
        let encode = |value: i64| {
            let mut writer = KeyWriter::new();
            writer.put_i64(value);
            writer.finish()
        };
        assert_eq!(encode(i64::MIN), vec![0x00; 8]);
        assert_eq!(encode(i64::MAX), vec![0xFF; 8]);
        assert!(encode(-1) < encode(0));
        assert!(encode(0) < encode(1));
        assert!(encode(i64::MIN) < encode(-1));
    }

    #[test]
    fn descending_u64_reverses_order() {
        let encode = |value: u64| {
            let mut writer = KeyWriter::new();
            writer.put_u64_descending(value);
            writer.finish()
        };
        assert!(encode(9) < encode(1));
        assert_eq!(encode(0), vec![0xFF; 8]);
        assert_eq!(encode(u64::MAX), vec![0x00; 8]);
    }

    #[test]
    fn a_truncated_read_names_the_kind_and_the_offset() {
        let mut reader = KeyReader::new(KeyKind::Record, &[0x01, 0x02]);
        let error = reader.take_u32().unwrap_err();
        match error {
            Error::Truncated {
                kind,
                offset,
                needed,
                available,
            } => {
                assert_eq!(kind, KeyKind::Record);
                assert_eq!(offset, 0);
                assert_eq!(needed, 4);
                assert_eq!(available, 2);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn an_unterminated_component_is_rejected() {
        let mut reader = KeyReader::new(KeyKind::Record, b"abc");
        assert!(matches!(
            reader.take_variable().unwrap_err(),
            Error::UnterminatedComponent { .. }
        ));
    }

    #[test]
    fn an_invalid_escape_is_rejected_with_its_offset() {
        let mut reader = KeyReader::new(KeyKind::Record, &[0x61, 0x00, 0x07]);
        match reader.take_variable().unwrap_err() {
            Error::InvalidEscape {
                offset,
                found,
                kind,
                ..
            } => {
                assert_eq!(kind, KeyKind::Record);
                assert_eq!(offset, 1);
                assert_eq!(found, 0x07);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_wrong_kind_tag_names_what_was_found() {
        let bytes = [KeyKind::LogEntry.tag()];
        let mut reader = KeyReader::new(KeyKind::Record, &bytes);
        match reader.expect_kind().unwrap_err() {
            Error::UnexpectedKind {
                expected,
                found,
                found_name,
            } => {
                assert_eq!(expected, KeyKind::Record);
                assert_eq!(found, KeyKind::LogEntry.tag());
                assert_eq!(found_name, "log-entry");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn an_unassigned_tag_is_reported_as_unassigned() {
        let mut reader = KeyReader::new(KeyKind::Record, &[0x7F]);
        match reader.expect_kind().unwrap_err() {
            Error::UnexpectedKind { found_name, .. } => assert_eq!(found_name, "unassigned"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn trailing_bytes_are_a_decode_failure() {
        let mut reader = KeyReader::new(KeyKind::Record, &[0x00, 0x00, 0x00, 0x01, 0xAA]);
        assert_eq!(reader.take_u32().unwrap(), 1);
        assert!(matches!(
            reader.finish().unwrap_err(),
            Error::TrailingBytes { extra: 1, .. }
        ));
    }

    #[test]
    fn every_primitive_round_trips() {
        let mut writer = KeyWriter::with_capacity(64);
        writer
            .put_u8(7)
            .put_u32(u32::MAX)
            .put_u64(1 << 40)
            .put_i64(-9)
            .put_u64_descending(5)
            .put_variable(b"with\x00zero")
            .put_fixed(&[1, 2, 3]);
        let bytes = writer.finish();

        let mut reader = KeyReader::new(KeyKind::Record, &bytes);
        assert_eq!(reader.take_u8().unwrap(), 7);
        assert_eq!(reader.take_u32().unwrap(), u32::MAX);
        assert_eq!(reader.take_u64().unwrap(), 1 << 40);
        assert_eq!(reader.take_i64().unwrap(), -9);
        assert_eq!(reader.take_u64_descending().unwrap(), 5);
        assert_eq!(reader.take_variable().unwrap(), b"with\x00zero".to_vec());
        assert_eq!(reader.take_fixed::<3>().unwrap(), [1, 2, 3]);
        reader.finish().unwrap();
    }
}
