use super::*;

/// One version of one record as the store holds it: what the record became, and
/// the causal context its writer had **seen** when it wrote.
///
/// The stamp sits beside the version rather than inside it because a deletion is
/// as capable of being concurrent as a write is — a delete racing a write to one
/// record is one of the cases a multi-master range has to name, and an enum
/// variant could only carry the stamp *instead of* `Tombstone`, never alongside
/// it (Q-638).
///
/// It is also the only home the field needs. `Mutation` carries a version, and
/// the record store holds these same bytes under a [`RecordKey`], so one field
/// here reaches the log, the wire, a backup and the store at once. Putting it on
/// `Mutation` would have served the log and left the store's codec to change
/// again later, and ADR-0059's rule is that a format door closes once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StampedValue {
    pub(super) value: RecordValue,
    stamp: CausalStamp,
    /// The millisecond this version stops being answered at, when it has one.
    pub(super) expires: Option<u64>,
    /// The transaction across leaders this version was resolved from, while
    /// its record stands (ADR-0112 D5).
    provenance: Option<Provenance>,
}

impl StampedValue {
    /// A version written with no recorded causal context.
    ///
    /// The honest value for a store that has never had two masters, in the same
    /// way [`LogRecord::new`] is the honest value for one that has never elected
    /// anybody: an empty stamp leaves the flag bit clear, so nothing already
    /// written is rewritten and an older build's bytes decode unchanged.
    #[must_use]
    pub fn new(value: RecordValue) -> Self {
        Self {
            value,
            stamp: CausalStamp::new(),
            expires: None,
            provenance: None,
        }
    }

    /// A version written by a node carrying what it had seen.
    #[must_use]
    pub const fn stamped(stamp: CausalStamp, value: RecordValue) -> Self {
        Self {
            value,
            stamp,
            expires: None,
            provenance: None,
        }
    }

    /// This version, as resolved from a transaction across leaders.
    #[must_use]
    pub fn from_transaction(mut self, provenance: Provenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// This version with no transaction named — what a settled transaction's
    /// version becomes once no read can tell it apart (ADR-0112, Q-922).
    #[must_use]
    pub fn settled(mut self) -> Self {
        self.provenance = None;
        self
    }

    /// The transaction across leaders this version came from, if any.
    #[must_use]
    pub const fn provenance(&self) -> Option<&Provenance> {
        self.provenance.as_ref()
    }

    /// What the record became at this version.
    #[must_use]
    pub const fn value(&self) -> &RecordValue {
        &self.value
    }

    /// The causal context the writer had seen, empty when none was recorded.
    #[must_use]
    pub const fn stamp(&self) -> &CausalStamp {
        &self.stamp
    }

    /// Take the version, discarding the stamp.
    #[must_use]
    pub fn into_value(self) -> RecordValue {
        self.value
    }
}

/// Split the optional causal stamp off an encoded version.
///
/// One splitter for the same reason [`log_record::split_epoch`] gives: two readings of one
/// byte string is a thing that can come to disagree with itself, and here the
/// disagreement would be about whether two writes saw each other.
pub(super) fn split_stamp(bytes: &[u8]) -> Result<(CausalStamp, u8, &[u8])> {
    let (flags, payload) = split_header(
        bytes,
        FLAG_TOMBSTONE | FLAG_STAMP | FLAG_EXPIRES | FLAG_ACROSS,
    )?;
    if flags & FLAG_STAMP == 0 {
        return Ok((CausalStamp::new(), flags, payload));
    }
    let raw: [u8; STAMP_COUNT_LEN] = payload
        .get(..STAMP_COUNT_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(STAMP_COUNT_LEN),
        })?;
    // A count wider than this target's `usize` cannot describe bytes that are
    // here, so it saturates and the length check below reports it truncated.
    let count = usize::try_from(u32::from_be_bytes(raw)).unwrap_or(usize::MAX);
    let span = count.saturating_mul(STAMP_ENTRY_LEN);
    let body = payload
        .get(STAMP_COUNT_LEN..STAMP_COUNT_LEN.saturating_add(span))
        .ok_or(Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN
                .saturating_add(STAMP_COUNT_LEN)
                .saturating_add(span),
        })?;
    let mut entries = Vec::with_capacity(count);
    for entry in body.as_chunks::<STAMP_ENTRY_LEN>().0 {
        let node: [u8; NODE_ID_LEN] = entry
            .get(..NODE_ID_LEN)
            .and_then(|head| head.try_into().ok())
            .ok_or(Error::ValueTruncated {
                len: bytes.len(),
                needed: STAMP_ENTRY_LEN,
            })?;
        let seen: [u8; 8] = entry
            .get(NODE_ID_LEN..)
            .and_then(|tail| tail.try_into().ok())
            .ok_or(Error::ValueTruncated {
                len: bytes.len(),
                needed: STAMP_ENTRY_LEN,
            })?;
        entries.push((node, u64::from_be_bytes(seen)));
    }
    Ok((
        CausalStamp::from_entries(entries)?,
        flags,
        payload
            .get(STAMP_COUNT_LEN.saturating_add(span)..)
            .unwrap_or_default(),
    ))
}

impl StoreValue for StampedValue {
    /// The stamp goes in front of the record's payload, not behind it.
    ///
    /// The same argument the epoch's position rests on: a reader that wants only
    /// the causal context reads a bounded prefix instead of walking a payload
    /// whose length it would otherwise have to learn from somewhere else.
    fn encode(&self) -> Value {
        let entries = self.stamp.entries();
        let mut flags = if self.value.is_tombstone() {
            FLAG_TOMBSTONE
        } else {
            0
        };
        if !entries.is_empty() {
            flags |= FLAG_STAMP;
        }
        // A deletion never expires: it is already the absence an expiry would
        // produce, so the instant is dropped rather than written beside it.
        let expires = self.expires.filter(|_| !self.value.is_tombstone());
        if expires.is_some() {
            flags |= FLAG_EXPIRES;
        }
        let stamp_len = if entries.is_empty() {
            0
        } else {
            STAMP_COUNT_LEN.saturating_add(entries.len().saturating_mul(STAMP_ENTRY_LEN))
        };
        let provenance = self.provenance.as_ref().map(|provenance| {
            let mut writer = KeyWriter::with_capacity(PROVENANCE_CAPACITY);
            across::put_provenance(&mut writer, provenance);
            writer.finish()
        });
        if provenance.is_some() {
            flags |= FLAG_ACROSS;
        }
        let payload = self.value.payload();
        let expires_len = if expires.is_some() { EXPIRES_LEN } else { 0 };
        let mut buffer = with_header(
            flags,
            stamp_len
                .saturating_add(expires_len)
                .saturating_add(provenance.as_ref().map_or(0, Vec::len))
                .saturating_add(payload.len()),
        );
        if !entries.is_empty() {
            buffer.extend_from_slice(
                &u32::try_from(entries.len())
                    .unwrap_or(u32::MAX)
                    .to_be_bytes(),
            );
            for (node, seen) in entries {
                buffer.extend_from_slice(node);
                buffer.extend_from_slice(&seen.to_be_bytes());
            }
        }
        if let Some(at) = expires {
            buffer.extend_from_slice(&at.to_be_bytes());
        }
        // After the expiry and before the payload, the order every other
        // optional field keeps: fixed-width prefixes first, then the body.
        if let Some(provenance) = &provenance {
            buffer.extend_from_slice(provenance);
        }
        buffer.extend_from_slice(payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (stamp, flags, rest) = split_stamp(bytes)?;
        let tombstone = flags & FLAG_TOMBSTONE != 0;
        let (expires, rest) = if flags & FLAG_EXPIRES == 0 {
            (None, rest)
        } else {
            if tombstone {
                // No writer puts an instant on a deletion, so one that carries it
                // was written by something this build does not understand.
                return Err(Error::ReservedFlags { flags });
            }
            let (expires, rest) = expiry::split(bytes.len(), rest)?;
            (Some(expires), rest)
        };
        let (provenance, payload) = if flags & FLAG_ACROSS == 0 {
            (None, rest)
        } else {
            let mut reader = KeyReader::new(crate::kind::KeyKind::Record, rest);
            let provenance = across::take_provenance(&mut reader)?;
            (
                Some(provenance),
                rest.get(reader.position()..).unwrap_or_default(),
            )
        };
        let value = if !tombstone {
            RecordValue::Present(payload.to_vec())
        } else if payload.is_empty() {
            RecordValue::Tombstone
        } else {
            return Err(Error::TombstoneWithPayload { len: payload.len() });
        };
        Ok(Self {
            value,
            stamp,
            expires,
            provenance,
        })
    }
}
