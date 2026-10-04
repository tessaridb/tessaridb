use super::*;

/// One suffix of one dictionary term (ADR-0105 D9, key kind `0x1f`).
///
/// ```text
/// <0x1f> <address> <suffix, variable> <term, variable>
/// ```
///
/// Every suffix of a term at least the prefix floor long has one entry, written
/// when the term enters the dictionary and deleted when it leaves, in the same
/// batch as the dictionary entry — so an infix is a **range read over
/// suffixes**: the suffixes beginning with the piece name exactly the terms
/// containing it, and no n-gram ever enters the analysis chain. The growth is
/// bounded by the dictionary, not by the text.
///
/// Both strings are the order-preserving variable encoding, so a walk bounded
/// by [`Self::piece_prefix`] reaches every suffix beginning with the piece and
/// nothing else. The value is empty: the entry's presence is the whole fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSuffixKey {
    /// Which index the term belongs to.
    pub address: IndexAddress,
    /// The suffix.
    pub suffix: String,
    /// The term it is a suffix of.
    pub term: String,
}

impl SearchSuffixKey {
    /// Name one suffix of one term.
    #[must_use]
    pub const fn new(address: IndexAddress, suffix: String, term: String) -> Self {
        Self {
            address,
            suffix,
            term,
        }
    }

    /// Where an index's suffixes live.
    #[must_use]
    pub const fn keyspace() -> tessari_kv::Keyspace {
        KeyKind::SearchSuffix.keyspace()
    }

    /// The bytes every suffix beginning with `piece` starts with.
    #[must_use]
    pub fn piece_prefix(address: &IndexAddress, piece: &str) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        writer.put_variable_unterminated(piece.as_bytes());
        let mut bytes = address.prefix(KeyKind::SearchSuffix);
        bytes.extend_from_slice(&writer.finish());
        bytes
    }

    /// The key.
    #[must_use]
    pub fn encode(&self) -> Key {
        let mut writer = KeyWriter::new();
        writer
            .put_variable(self.suffix.as_bytes())
            .put_variable(self.term.as_bytes());
        let mut bytes = self.address.prefix(KeyKind::SearchSuffix);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    /// Read a key back.
    ///
    /// # Errors
    ///
    /// Returns an error for bytes that are not a suffix key, or whose strings
    /// are not text.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(KeyKind::SearchSuffix, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let suffix = String::from_utf8(reader.take_variable()?).map_err(|_| {
            crate::error::Error::InvalidUtf8 {
                kind: KeyKind::SearchSuffix,
            }
        })?;
        let term = String::from_utf8(reader.take_variable()?).map_err(|_| {
            crate::error::Error::InvalidUtf8 {
                kind: KeyKind::SearchSuffix,
            }
        })?;
        reader.finish()?;
        Ok(Self {
            address,
            suffix,
            term,
        })
    }

    /// The value every suffix entry holds: nothing beyond the header.
    #[must_use]
    pub fn empty() -> Value {
        Value::from(with_header(0, 0))
    }
}

/// One surface form of one stemmed term, and how many records hold that pair
/// (Q-867, key kind `0x42`).
///
/// ```text
/// <0x42> <address> <surface, variable> <term, variable>  →  <count u64>
/// ```
///
/// The raw companion of a stemmed dictionary. A stem is not a spelling anybody
/// wrote, so a misspelling is measured against the words the text actually
/// held: a fuzzy walk reads the surfaces sharing the typed word's first
/// letters and learns, from each one within the edit budget, which term to ask
/// the postings for. Written only where the stemmer changed the word — where it
/// did not, the term is its own surface and the term dictionary answers.
///
/// Counted for the reason the term dictionary is: a pair no record holds is
/// deleted, so a walk never offers a word nothing contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSurfaceKey {
    /// Which index the pair belongs to.
    pub address: IndexAddress,
    /// The word as the text held it, folded and unstemmed.
    pub surface: String,
    /// The term it stems to.
    pub term: String,
}

impl SearchSurfaceKey {
    /// Name one pair.
    #[must_use]
    pub const fn new(address: IndexAddress, surface: String, term: String) -> Self {
        Self {
            address,
            surface,
            term,
        }
    }

    /// Where an index's surfaces live.
    #[must_use]
    pub const fn keyspace() -> tessari_kv::Keyspace {
        KeyKind::SearchSurface.keyspace()
    }

    /// The bytes every surface beginning with `leading` starts with.
    #[must_use]
    pub fn surface_prefix(address: &IndexAddress, leading: &str) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        writer.put_variable_unterminated(leading.as_bytes());
        let mut bytes = address.prefix(KeyKind::SearchSurface);
        bytes.extend_from_slice(&writer.finish());
        bytes
    }

    /// The entry saying an index's surfaces are complete: written when the
    /// index is built, so an index built before surfaces were kept — which
    /// holds none — is told apart from one whose text simply never stemmed.
    #[must_use]
    pub fn marker(address: IndexAddress) -> Key {
        Self::new(address, String::new(), String::new()).encode()
    }

    /// The bytes every surface of one index starts with.
    #[must_use]
    pub fn index_prefix(address: &IndexAddress) -> Vec<u8> {
        address.prefix(KeyKind::SearchSurface)
    }

    /// The key.
    #[must_use]
    pub fn encode(&self) -> Key {
        let mut writer = KeyWriter::new();
        writer
            .put_variable(self.surface.as_bytes())
            .put_variable(self.term.as_bytes());
        let mut bytes = self.address.prefix(KeyKind::SearchSurface);
        bytes.extend_from_slice(&writer.finish());
        Key::from(bytes)
    }

    /// Read a key back.
    ///
    /// # Errors
    ///
    /// Returns an error for bytes that are not a surface key, or whose strings
    /// are not text.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let text = |held: Vec<u8>| {
            String::from_utf8(held).map_err(|_| crate::error::Error::InvalidUtf8 {
                kind: KeyKind::SearchSurface,
            })
        };
        let mut reader = KeyReader::new(KeyKind::SearchSurface, bytes);
        reader.expect_kind()?;
        let address = IndexAddress::read(&mut reader)?;
        let surface = text(reader.take_variable()?)?;
        let term = text(reader.take_variable()?)?;
        reader.finish()?;
        Ok(Self {
            address,
            surface,
            term,
        })
    }

    /// The value recording how many records hold the pair.
    #[must_use]
    pub fn count(held: u64) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u64(held);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    /// The count a stored value records.
    ///
    /// # Errors
    ///
    /// Returns an error for a value that is not a count.
    pub fn counted(bytes: &[u8]) -> Result<u64> {
        let (_, payload) = split_header(bytes, 0)?;
        let mut reader = KeyReader::new(KeyKind::SearchSurface, payload);
        let held = reader.take_u64()?;
        reader.finish()?;
        Ok(held)
    }
}
