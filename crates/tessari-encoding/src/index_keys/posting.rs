use super::*;

/// What a term does in one record: how often it occurs, and how long the record
/// is.
///
/// # Why a posting carries the record's length, which is not a property of the
/// term
///
/// A relevance score needs four numbers. Two describe the collection — how many
/// records there are and how long a typical one is — and are held once, beside
/// the postings, in [`SearchStatistics`]. The other two describe *this* record:
/// how often it holds the term, and how long it is.
///
/// The frequency plainly belongs here. The length is a property of the record,
/// so the tidy place for it would be one entry per record — and it is here
/// instead, repeated once per distinct term. That is deliberate: it makes a
/// score computable from the postings scan **alone**. A scan of one term's
/// postings yields the record, the frequency and the length together, so
/// scoring costs no further read at all, where a separate length entry would
/// cost one point read per candidate — most of what storing the numbers was
/// meant to remove.
///
/// The redundancy also cannot drift. A record's update already deletes its whole
/// posting set and writes a new one, so a changed length rewrites exactly the
/// postings that were being rewritten anyway, by the same code, in the same
/// batch.
///
/// # A posting written before this payload existed
///
/// [`Self::Membership`] is what an older format wrote: the header and nothing
/// after it. It says the term is in the record and no more, which is all
/// `MATCHES` ever needed — so an index written that way keeps answering
/// `MATCHES` correctly and only cannot be **scored**. The distinction is carried
/// by the encoding itself rather than by a declared version, so it cannot
/// disagree with the data it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posting {
    /// The term is in the record. Written before postings carried a payload.
    Membership,
    /// The term is in the record this often, and the record is this long.
    Counted {
        /// Occurrences of this term in this record, **with** repeats.
        frequency: u32,
        /// Tokens in the record's analysed field, **with** repeats.
        ///
        /// The same quantity [`SearchStatistics::terms`] accumulates, so the two
        /// cannot mean different things by "length".
        length: u32,
    },
}

impl StoreValue for Posting {
    fn encode(&self) -> Value {
        let Self::Counted { frequency, length } = *self else {
            // Byte-identical to `NoPayload`, because it is the same statement.
            return Value::from(with_header(0, 0));
        };
        let mut writer = KeyWriter::new();
        writer.put_u32(frequency).put_u32(length);
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        if payload.is_empty() {
            return Ok(Self::Membership);
        }
        let mut reader = KeyReader::new(KeyKind::Posting, payload);
        let frequency = reader.take_u32()?;
        let length = reader.take_u32()?;
        // Whatever lists follow are read — and so checked — here too, so a
        // posting that decodes as counted is one whose whole payload is sound.
        take_lists(&mut reader, frequency)?;
        reader.finish()?;
        Ok(Self::Counted { frequency, length })
    }
}

/// The positions flag: the posting lists the term's token ordinals.
pub(super) const LISTS_POSITIONS: u8 = 1;

/// The offsets flag: the posting lists the term's byte ranges.
pub(super) const LISTS_OFFSETS: u8 = 2;

/// The fields flag: the posting lists a frequency and a length per member
/// field, after a one-byte field count.
pub(super) const LISTS_FIELDS: u8 = 4;

/// Where one term sits in one record, as a `POSITIONS` / `OFFSETS` index keeps
/// it (ADR-0100 D4).
///
/// Empty lists are what an index without those options holds, so a reader asks
/// one question whatever the index declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Located {
    /// The term's token ordinals in the record, ascending.
    pub positions: Vec<u32>,
    /// The term's byte ranges in the record's text, start inclusive and end
    /// exclusive, in token order.
    pub offsets: Vec<(u32, u32)>,
    /// For a search member, per field in declaration order: how often the term
    /// occurs in that field and how long that field is (Q-870). Empty for a
    /// field index, and for a member posting written before it was kept.
    ///
    /// What lets a `FROM SEARCH` score BM25F — each field normalised by its own
    /// length — from the postings alone, where without it every candidate's
    /// text had to be read and analysed again.
    pub fields: Vec<(u32, u32)>,
}

impl Posting {
    /// A counted posting with the lists an option asked for.
    ///
    /// The payload is the counted one — frequency and length — then, only when
    /// a list is present, a flags byte naming which follow and the lists
    /// themselves, `frequency` entries each. A posting with neither list is
    /// byte-identical to [`Posting::Counted`]'s encoding, so an index with no
    /// option writes exactly what it always wrote.
    #[must_use]
    pub fn encode_located(frequency: u32, length: u32, located: &Located) -> Value {
        let mut writer = KeyWriter::new();
        writer.put_u32(frequency).put_u32(length);
        let mut flags = 0_u8;
        if !located.positions.is_empty() {
            flags |= LISTS_POSITIONS;
        }
        if !located.offsets.is_empty() {
            flags |= LISTS_OFFSETS;
        }
        // A member declares at most a byte's worth of fields; one with more
        // keeps the counted payload, which every reader still scores from text.
        let fields = u8::try_from(located.fields.len())
            .ok()
            .filter(|count| *count > 0);
        if fields.is_some() {
            flags |= LISTS_FIELDS;
        }
        if flags != 0 {
            writer.put_u8(flags);
            for position in &located.positions {
                writer.put_u32(*position);
            }
            for (start, end) in &located.offsets {
                writer.put_u32(*start).put_u32(*end);
            }
            if let Some(count) = fields {
                writer.put_u8(count);
                for (frequency, length) in &located.fields {
                    writer.put_u32(*frequency).put_u32(*length);
                }
            }
        }
        let body = writer.finish();
        let mut buffer = with_header(0, body.len());
        buffer.extend_from_slice(&body);
        Value::from(buffer)
    }

    /// The lists a stored posting carries — empty for one written without them.
    ///
    /// # Errors
    ///
    /// Returns an error for a payload that does not read whole: an unknown flag,
    /// a list shorter or longer than the frequency says.
    pub fn located(bytes: &[u8]) -> Result<Located> {
        let (_, payload) = split_header(bytes, 0)?;
        if payload.is_empty() {
            return Ok(Located::default());
        }
        let mut reader = KeyReader::new(KeyKind::Posting, payload);
        let frequency = reader.take_u32()?;
        reader.take_u32()?;
        let located = take_lists(&mut reader, frequency)?;
        reader.finish()?;
        Ok(located)
    }
}

/// The optional lists after a counted payload, each `frequency` entries long.
pub(super) fn take_lists(reader: &mut KeyReader<'_>, frequency: u32) -> Result<Located> {
    if reader.remaining() == 0 {
        return Ok(Located::default());
    }
    let flags = reader.take_u8()?;
    if flags & !(LISTS_POSITIONS | LISTS_OFFSETS | LISTS_FIELDS) != 0 || flags == 0 {
        return Err(Error::ReservedFlags { flags });
    }
    let count = usize::try_from(frequency).unwrap_or(usize::MAX);
    let mut located = Located::default();
    if flags & LISTS_POSITIONS != 0 {
        located.positions.reserve(count.min(reader.remaining()));
        for _ in 0..count {
            located.positions.push(reader.take_u32()?);
        }
    }
    if flags & LISTS_OFFSETS != 0 {
        located.offsets.reserve(count.min(reader.remaining()));
        for _ in 0..count {
            let start = reader.take_u32()?;
            located.offsets.push((start, reader.take_u32()?));
        }
    }
    if flags & LISTS_FIELDS != 0 {
        let count = reader.take_u8()?;
        for _ in 0..count {
            let frequency = reader.take_u32()?;
            located.fields.push((frequency, reader.take_u32()?));
        }
    }
    Ok(located)
}

/// Walk the field list and keep its bytes verbatim.
///
/// The fields are not decoded — the encoding normalises numbers and so cannot be
/// reversed — but the walk still has to be exact, because whatever follows the
/// list starts where the walk stops.
pub(super) fn take_values(reader: &mut KeyReader<'_>) -> Result<IndexValues> {
    let start = reader.position();
    while reader.peek()? != index_value::END {
        index_value::skip(reader)?;
    }
    reader.take_u8()?;
    Ok(IndexValues(reader.consumed_since(start).to_vec()))
}
