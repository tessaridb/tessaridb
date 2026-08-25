//! Keys, values and key ranges.
//!
//! Keys are opaque byte strings ordered lexicographically. The substrate
//! attaches no meaning to their contents — the key *grammar* (which bytes encode
//! a namespace, a table, an index entry) belongs to the storage layer above.

use std::ops::Bound;

/// An ordered, opaque byte key.
///
/// Ordering is lexicographic over the raw bytes, which is what makes range
/// scans and index iteration possible. Any structure inside the key is imposed
/// by the layer that built it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Key(Vec<u8>);

impl Key {
    /// Wrap owned bytes as a key.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Copy a byte slice into a new key.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    /// The raw bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Consume the key and return its bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the key has no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Key {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<&[u8]> for Key {
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

/// Keys are rendered as readable text when they are valid UTF-8, and as hex
/// otherwise, so an error message about a key is useful in both cases.
impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match std::str::from_utf8(&self.0) {
            Ok(text) => write!(f, "{text}"),
            Err(_) => {
                f.write_str("0x")?;
                for byte in &self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}

/// An opaque byte value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Value(Vec<u8>);

impl Value {
    /// Wrap owned bytes as a value.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Copy a byte slice into a new value.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }

    /// The raw bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Consume the value and return its bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the value has no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Value {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<&[u8]> for Value {
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

/// A half-open or bounded span of the key space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    start: Bound<Key>,
    end: Bound<Key>,
}

impl KeyRange {
    /// Every key in the store.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            start: Bound::Unbounded,
            end: Bound::Unbounded,
        }
    }

    /// Keys from `start` inclusive up to `end` exclusive.
    #[must_use]
    pub fn between(start: Key, end: Key) -> Self {
        Self {
            start: Bound::Included(start),
            end: Bound::Excluded(end),
        }
    }

    /// Keys carrying the given prefix.
    ///
    /// The upper bound is the prefix with its last non-`0xff` byte incremented,
    /// which is the smallest key greater than every key with this prefix. A
    /// prefix that is empty or entirely `0xff` has no such successor, so the
    /// range runs to the end of the key space.
    #[must_use]
    pub fn prefix(prefix: &[u8]) -> Self {
        let start = Key::from_slice(prefix);
        match prefix_upper_bound(prefix) {
            Some(upper) => Self {
                start: Bound::Included(start),
                end: Bound::Excluded(Key::new(upper)),
            },
            None => Self {
                start: Bound::Included(start),
                end: Bound::Unbounded,
            },
        }
    }

    /// Build a range from explicit bounds.
    #[must_use]
    pub const fn from_bounds(start: Bound<Key>, end: Bound<Key>) -> Self {
        Self { start, end }
    }

    /// The lower bound.
    #[must_use]
    pub const fn start(&self) -> &Bound<Key> {
        &self.start
    }

    /// The upper bound.
    #[must_use]
    pub const fn end(&self) -> &Bound<Key> {
        &self.end
    }

    /// This range, continued strictly after `last`.
    ///
    /// The upper bound is kept and the lower one becomes `last` exclusive, so a
    /// walk resuming here sees every remaining key of the original range and
    /// re-reads none of what it already had.
    ///
    /// # Why the bound rather than a successor key
    ///
    /// The other way to say "after this" is to append a zero byte and include
    /// it, which is what the LSM backend does internally when it has to hand an
    /// engine a lower bound. Said here it would be a second implementation of
    /// byte-order successor arithmetic, in a type that already has an exclusive
    /// bound meaning exactly this — and the two would have to agree forever. An
    /// `Excluded` bound is the same statement made once, and each backend
    /// already translates it.
    #[must_use]
    pub fn resuming_after(&self, last: &Key) -> Self {
        Self {
            start: Bound::Excluded(last.clone()),
            end: self.end.clone(),
        }
    }

    /// Whether the bounds can never contain a key.
    ///
    /// This is a request-shape check, not a lookup: it catches an inverted or
    /// empty span before a backend walks anything.
    #[must_use]
    pub fn is_provably_empty(&self) -> bool {
        match (&self.start, &self.end) {
            (Bound::Included(lo), Bound::Excluded(hi)) => lo >= hi,
            (Bound::Included(lo), Bound::Included(hi))
            | (Bound::Excluded(lo), Bound::Excluded(hi)) => lo > hi,
            (Bound::Excluded(lo), Bound::Included(hi)) => lo >= hi,
            _ => false,
        }
    }
}

/// The smallest key strictly greater than every key carrying `prefix`.
///
/// Returns `None` when no such key exists — an empty prefix, or one made
/// entirely of `0xff` bytes, both run to the end of the key space.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    while let Some(last) = upper.pop() {
        if let Some(next) = last.checked_add(1) {
            upper.push(next);
            return Some(upper);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn keys_order_lexicographically() {
        let mut keys = [
            Key::from_slice(b"b"),
            Key::from_slice(b"a"),
            Key::from_slice(b"ab"),
            Key::from_slice(b"aa"),
        ];
        keys.sort();
        let rendered: Vec<String> = keys.iter().map(ToString::to_string).collect();
        assert_eq!(rendered, ["a", "aa", "ab", "b"]);
    }

    #[test]
    fn key_display_falls_back_to_hex_for_non_utf8() {
        let key = Key::new(vec![0xff, 0x00]);
        assert_eq!(key.to_string(), "0xff00");
    }

    #[test]
    fn prefix_range_excludes_the_next_prefix() {
        let range = KeyRange::prefix(b"user:");
        assert_eq!(range.start(), &Bound::Included(Key::from_slice(b"user:")));
        assert_eq!(range.end(), &Bound::Excluded(Key::from_slice(b"user;")));
    }

    #[test]
    fn all_ff_prefix_runs_to_the_end_of_the_key_space() {
        let range = KeyRange::prefix(&[0xff, 0xff]);
        assert_eq!(range.end(), &Bound::Unbounded);
    }

    #[test]
    fn empty_prefix_runs_to_the_end_of_the_key_space() {
        let range = KeyRange::prefix(b"");
        assert_eq!(range.end(), &Bound::Unbounded);
    }

    /// A resumed range excludes the key it resumed from, and keeps its end.
    ///
    /// # Why this is tested here and not through a walk that uses it
    ///
    /// It was tried the other way first, and the test could not fail. Every
    /// caller of a batched walk in this engine collects into a `BTreeSet` or a
    /// `BTreeMap` keyed by record identity, so a seam that hands the same entry
    /// back twice is absorbed before anything observable — an inclusive resume
    /// produces exactly the right answer, one duplicate per batch more slowly.
    ///
    /// A property that the code downstream of it deduplicates cannot be tested
    /// downstream of that code. It is asserted here, where it is the whole
    /// content of the method, and where the assertion fails the moment the bound
    /// stops being exclusive.
    #[test]
    fn a_resumed_range_starts_after_the_key_it_stopped_on() {
        let range = KeyRange::between(Key::from_slice(b"a"), Key::from_slice(b"z"));
        let resumed = range.resuming_after(&Key::from_slice(b"m"));
        assert_eq!(
            resumed.start(),
            &Bound::Excluded(Key::from_slice(b"m")),
            "an inclusive resume re-reads the entry the previous batch ended on"
        );
        assert_eq!(resumed.end(), range.end(), "the end of the range moved");
    }

    #[test]
    fn inverted_and_empty_spans_are_provably_empty() {
        assert!(
            KeyRange::between(Key::from_slice(b"b"), Key::from_slice(b"a")).is_provably_empty()
        );
        assert!(
            KeyRange::between(Key::from_slice(b"a"), Key::from_slice(b"a")).is_provably_empty()
        );
        assert!(
            !KeyRange::between(Key::from_slice(b"a"), Key::from_slice(b"b")).is_provably_empty()
        );
        assert!(!KeyRange::all().is_provably_empty());
    }
}
