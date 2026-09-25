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

    /// What a reader whose clock reads `now` sees of this version.
    ///
    /// An expired version reads as a deletion, which is what every read path
    /// already knows how to skip — so a caller that asks here cannot answer with
    /// an expired value by forgetting a second check.
    #[must_use]
    pub fn into_visible_at(self, now: u64) -> RecordValue {
        if self.is_expired_at(now) {
            RecordValue::Tombstone
        } else {
            self.value
        }
    }
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
