//! The monotonic term bloom one search index carries (key kind `0x4b`).
//!
//! ```text
//! <0x4b> <address>
//! ```
//!
//! The value holds the bloom bits over every term's payload encoding, and the
//! estimate of how many terms those bits describe. One key per index — the
//! address is the whole key, so finding it is a point read.
//!
//! # Monotonic, on purpose
//!
//! Terms enter the dictionary and leave it; the bloom only ever gains bits.
//! A term reaching zero deletes its dictionary entry but keeps its bloom
//! bits, so a stale bit costs a read and a cleared bit would cost a row — a
//! negative answer must never be produced for a term the dictionary holds,
//! because the read trusts it and skips the posting walk. False positives
//! only, which is the direction an approximate structure must err in.

use super::IndexAddress;
use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::value::StoreValue;
use crate::value::{split_header, with_header};
use tessari_kv::{Key, Value};

/// Where one search index's term bloom lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBloomKey {
    /// Which index this bloom belongs to.
    pub address: IndexAddress,
}

impl SearchBloomKey {
    /// Name one index's term bloom.
    #[must_use]
    pub const fn new(address: IndexAddress) -> Self {
        Self { address }
    }

    /// The bytes every bloom key an index might read starts with — the key
    /// itself, since the address is the whole key.
    #[must_use]
    pub fn key_prefix(address: &IndexAddress) -> Vec<u8> {
        address.prefix(KeyKind::SearchBloom)
    }
}

impl StoreKey for SearchBloomKey {
    type Value = SearchBloom;

    const KIND: KeyKind = KeyKind::SearchBloom;

    fn encode(&self) -> Key {
        Key::from(Self::key_prefix(&self.address))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { address })
    }
}

/// The bloom bits and the term-count estimate, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchBloom {
    /// How many terms the bits were built from, as folded so far.
    ///
    /// The estimate only grows, like the bits; it exists so a reader can tell
    /// when the filter has outgrown its usefulness (past the capacity the
    /// constants name) and stop consulting it rather than read through a
    /// filter that denies nothing.
    pub terms: u64,
    /// The bloom bits, in the byte form the filter structure encodes.
    pub bits: Vec<u8>,
}

impl SearchBloom {
    /// State the estimate and the bits.
    #[must_use]
    pub fn new(terms: u64, bits: Vec<u8>) -> Self {
        Self { terms, bits }
    }
}

impl StoreValue for SearchBloom {
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u64(self.terms);
        writer
            .put_u32(u32::try_from(self.bits.len()).unwrap_or(u32::MAX))
            .put_fixed(&self.bits);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SearchBloom, payload);
        let terms = reader.take_u64()?;
        let len = usize::try_from(reader.take_u32()?).unwrap_or(usize::MAX);
        let bits = reader.take_exact(len)?;
        reader.finish()?;
        Ok(Self { terms, bits })
    }
}
