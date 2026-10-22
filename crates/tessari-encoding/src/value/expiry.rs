//! A record version that stops being answered at a stated instant (G035).
//!
//! # A reader compares; nothing decides for it
//!
//! The instant is written into the version by the session that wrote the value,
//! once, as milliseconds since the Unix epoch. Whether the version is still
//! visible is then a comparison every reader makes against its own clock, the
//! rule the queue's hold already follows: a replica applies the bytes it was
//! sent rather than asking its own clock whether to keep them, and a removal
//! pass that lags, is throttled or never runs costs storage, never an answer.
//!
//! The comparison is `expires <= now`: a version is gone **at** its instant, not
//! a millisecond after it, so an expiry equal to the reader's clock is already
//! past and "expire in zero" can never be read back.

use super::{EXPIRES_LEN, RecordValue, StampedValue};
use crate::error::{Error, Result};

/// Bytes a count, and a field name's length, take in a field-instant list.
const COUNT_LEN: usize = 4;

/// Each field given an instant, and the instant, in milliseconds.
type FieldInstants = Vec<(String, u64)>;

impl StampedValue {
    /// This version, answered only until `at` milliseconds since the epoch.
    ///
    /// A deletion ignores it: it is already the absence an expiry produces, and
    /// the encoder drops the instant rather than writing it beside a tombstone.
    #[must_use]
    pub fn expiring(mut self, at: u64) -> Self {
        self.expires = Some(at);
        self
    }

    /// The millisecond this version stops being answered at, when it has one.
    #[must_use]
    pub const fn expires(&self) -> Option<u64> {
        match self.value {
            RecordValue::Present(_) => self.expires,
            RecordValue::Tombstone => None,
        }
    }

    /// Whether a reader whose clock reads `now` must treat this version as gone.
    #[must_use]
    pub const fn is_expired_at(&self, now: u64) -> bool {
        match self.expires() {
            Some(at) => at <= now,
            None => false,
        }
    }

    /// This version with `fields` each answered only until its instant —
    /// sorted by field, the last instant named for a field kept (ADR-0132).
    #[must_use]
    pub fn with_field_expiry(mut self, mut fields: Vec<(String, u64)>) -> Self {
        fields.reverse();
        fields.sort_by(|left, right| left.0.cmp(&right.0));
        fields.dedup_by(|later, kept| later.0 == kept.0);
        self.field_expiry = fields;
        self
    }

    /// The instant each of some of its hash's fields stops being answered at,
    /// sorted by field; empty for a version that gives no field one.
    #[must_use]
    pub fn field_expiry(&self) -> &[(String, u64)] {
        match self.value {
            RecordValue::Present(_) => &self.field_expiry,
            RecordValue::Tombstone => &[],
        }
    }

    /// What a reader whose clock reads `now` sees of this version.
    ///
    /// An expired version reads as a deletion, which is what every read path
    /// already knows how to skip — so a caller that asks here cannot answer with
    /// an expired value by forgetting a second check. A field past its own
    /// instant is left out of the hash for the same reason, and a hash whose
    /// every field is past reads as a deletion too (ADR-0132 D2).
    #[must_use]
    pub fn into_visible_at(self, now: u64) -> RecordValue {
        if self.is_expired_at(now) {
            return RecordValue::Tombstone;
        }
        if !self.field_expiry.iter().any(|(_, at)| *at <= now) {
            return self.value;
        }
        let RecordValue::Present(payload) = &self.value else {
            return self.value;
        };
        // Only a hash is given field instants; anything else is answered as
        // it is, and a payload that does not decode is left for the reader's
        // own decode to report.
        let Ok(tessari_types::Value::Object(mut fields)) = crate::decode_payload(payload) else {
            return self.value;
        };
        for (field, at) in &self.field_expiry {
            if *at <= now {
                fields.remove(field);
            }
        }
        if fields.is_empty() {
            return RecordValue::Tombstone;
        }
        RecordValue::Present(
            crate::encode_payload(&tessari_types::Value::Object(fields))
                .as_slice()
                .to_vec(),
        )
    }
}

/// The bytes of a field-instant list: a count, then each field's name length,
/// name and instant. Empty for an empty list, which then takes no flag.
pub(super) fn fields_written(fields: &[(String, u64)]) -> Vec<u8> {
    if fields.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    out.extend_from_slice(
        &u32::try_from(fields.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    for (field, at) in fields {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(field.as_bytes());
        out.extend_from_slice(&at.to_be_bytes());
    }
    out
}

/// Split a field-instant list off the front of what follows the key instant.
pub(super) fn split_fields(whole: usize, rest: &[u8]) -> Result<(FieldInstants, &[u8])> {
    let truncated = |at: usize, needed: usize| Error::ValueTruncated {
        len: whole,
        needed: whole
            .saturating_sub(rest.len())
            .saturating_add(at)
            .saturating_add(needed),
    };
    let take = |at: usize, len: usize| {
        rest.get(at..at.saturating_add(len))
            .ok_or_else(|| truncated(at, len))
    };
    let count_of = |bytes: &[u8]| {
        let head: [u8; COUNT_LEN] = bytes.try_into().unwrap_or_default();
        usize::try_from(u32::from_be_bytes(head)).unwrap_or(usize::MAX)
    };
    let count = count_of(take(0, COUNT_LEN)?);
    let mut at = COUNT_LEN;
    // A count cannot promise more fields than there are bytes for.
    let mut fields = Vec::with_capacity(count.min(rest.len()));
    for _ in 0..count {
        let len = count_of(take(at, COUNT_LEN)?);
        at = at.saturating_add(COUNT_LEN);
        let name = std::str::from_utf8(take(at, len)?)
            .map_err(|_| Error::ValueTruncated {
                len: whole,
                needed: whole,
            })?
            .to_owned();
        at = at.saturating_add(len);
        let instant: [u8; EXPIRES_LEN] = take(at, EXPIRES_LEN)?.try_into().unwrap_or_default();
        at = at.saturating_add(EXPIRES_LEN);
        fields.push((name, u64::from_be_bytes(instant)));
    }
    Ok((fields, rest.get(at..).unwrap_or_default()))
}

/// Split an expiry instant off the front of what follows the stamp.
///
/// `whole` is the length of the stored value, for the truncation error.
pub(super) fn split(whole: usize, rest: &[u8]) -> Result<(u64, &[u8])> {
    let head: [u8; EXPIRES_LEN] = rest
        .get(..EXPIRES_LEN)
        .and_then(|head| head.try_into().ok())
        .ok_or(Error::ValueTruncated {
            len: whole,
            needed: whole.saturating_sub(rest.len()).saturating_add(EXPIRES_LEN),
        })?;
    Ok((
        u64::from_be_bytes(head),
        rest.get(EXPIRES_LEN..).unwrap_or_default(),
    ))
}
