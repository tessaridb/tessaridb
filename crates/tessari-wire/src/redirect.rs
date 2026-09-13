//! Where a read should have gone, in the form a client can act on.
//!
//! # Why this is a frame and not an error
//!
//! A redirect is an **instruction**, and a refusal is a failure. A client that
//! handles failures correctly — logs them, retries a bounded number of times,
//! gives up — handles an instruction encoded as one *incorrectly*, every time,
//! by construction. So [`crate::Destination::There`] leaves this node as its own
//! kind of frame rather than as a refusal carrying a hint.
//!
//! # Why the epoch and the settlement are here before anything reads them
//!
//! Both fields are inert in this build: nothing sends an `Elsewhere` frame yet,
//! so nothing checks the epoch on arrival and nothing acts on the settlement.
//! They are here because the cost of a frame field is paid when a client exists
//! that does not know it, not when it is written. Adding either later would be a
//! protocol change against builds in somebody's hands; adding both now is free.
//!
//! The **epoch** is what makes a redirect checkable. An address alone cannot be
//! stale-checked: a client that followed a redirect written three leaderships ago
//! would arrive, be redirected again, and have no way to tell a loop from
//! progress. Dated, the receiver can refuse it and say which leadership is
//! current, which is the same shape Raft gives a client that reaches the wrong
//! server.
//!
//! The **settlement** is the difference between *this is where it lives now* and
//! *go here for this one read*. A client that remembers the second as if it were
//! the first pins its map to an arrangement that was never meant to outlast the
//! request.

use tessari_encoding::NODE_ID_LEN;
use tessari_types::Epoch;

use crate::error::{Error, Result};
use crate::frame;

/// Whether a redirect is a fact about where the data lives, or about this read.
///
/// Two named values rather than a `bool`, and the reason is what a `bool` does at
/// the decode: every byte would have to mean one of the two, so a value this
/// build does not assign would silently become one of them. Named, an unknown
/// byte is refused and says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Settlement {
    /// Where the data lives now. A client may remember it.
    Settled,
    /// Where to go for this read only. A client must not remember it.
    Transient,
}

impl Settlement {
    /// Numbered explicitly and never renumbered, for the reason the frame tags
    /// are: a byte that once meant one thing cannot be asked about afterwards by
    /// a client of a different build.
    const fn byte(self) -> u8 {
        match self {
            Self::Settled => 1,
            Self::Transient => 2,
        }
    }

    /// Zero is deliberately unassigned: it is what a zeroed or truncated buffer
    /// holds, so giving it a meaning would make corruption decode as a value.
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Settled),
            2 => Some(Self::Transient),
            _ => None,
        }
    }
}

/// Everything before the address: the node id, the epoch, the settlement byte,
/// and the four-byte length the address carries in front of itself.
///
/// A constant rather than an expression at the call site, because the sum of a
/// handful of widths is exactly the arithmetic that is obvious until one of the
/// widths changes.
const FIXED: usize = NODE_ID_LEN + 8 + 1 + 4;

/// A read this node did not answer, and where it should go instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elsewhere {
    /// The address to dial — the same string the declaration carried.
    pub endpoint: String,
    /// Who is expected there, so a client that meets somebody else can notice.
    pub node: [u8; NODE_ID_LEN],
    /// The leadership the deciding node believed current when it decided.
    pub epoch: Epoch,
    /// Whether this is where the data lives, or where to go just this once.
    pub settlement: Settlement,
}

impl Elsewhere {
    /// The body of a [`crate::frame::Kind::Elsewhere`] frame.
    ///
    /// Fixed-width fields first and the address last, so every offset before it
    /// is known without reading anything.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(FIXED.saturating_add(self.endpoint.len()));
        body.extend_from_slice(&self.node);
        frame::put_u64(&mut body, self.epoch.get());
        body.push(self.settlement.byte());
        frame::put_text(&mut body, &self.endpoint);
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body is not the shape a redirect
    /// takes — including an empty address, which names nowhere to go and is a
    /// redirect only in shape — and [`Error::UnknownSettlement`] when it is the
    /// right shape but carries a settlement this build does not assign.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut node = [0_u8; NODE_ID_LEN];
        let head = body.get(..NODE_ID_LEN).ok_or(Error::Malformed)?;
        node.copy_from_slice(head);

        let (epoch, at) = frame::take_u64(body, NODE_ID_LEN)?;
        let byte = *body.get(at).ok_or(Error::Malformed)?;
        let settlement = Settlement::from_byte(byte).ok_or(Error::UnknownSettlement { byte })?;
        let (endpoint, _) = frame::take_text(body, at.checked_add(1).ok_or(Error::Malformed)?)?;
        if endpoint.is_empty() {
            return Err(Error::Malformed);
        }

        Ok(Self {
            endpoint,
            node,
            epoch: Epoch::new(epoch),
            settlement,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THERE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];

    fn redirect(settlement: Settlement) -> Elsewhere {
        Elsewhere {
            endpoint: "10.0.0.4:9080".to_owned(),
            node: THERE,
            epoch: Epoch::new(19),
            settlement,
        }
    }

    #[test]
    fn a_redirect_survives_the_wire_with_every_field_intact() {
        let sent = redirect(Settlement::Settled);
        let heard = Elsewhere::decode(&sent.encode()).expect("a redirect this build wrote");
        assert_eq!(heard, sent);
    }

    #[test]
    fn a_transient_redirect_does_not_arrive_as_a_settled_one() {
        // The one confusion that matters: a client remembering a one-read
        // redirect pins its map to an arrangement nobody meant to last.
        let heard = Elsewhere::decode(&redirect(Settlement::Transient).encode())
            .expect("a redirect this build wrote");
        assert_eq!(heard.settlement, Settlement::Transient);
    }

    #[test]
    fn a_truncated_redirect_is_refused_rather_than_half_read() {
        let whole = redirect(Settlement::Settled).encode();
        for cut in 0..whole.len() {
            let short = whole.get(..cut).expect("a prefix of a buffer");
            assert!(
                Elsewhere::decode(short).is_err(),
                "a body cut at {cut} of {} decoded",
                whole.len()
            );
        }
    }

    #[test]
    fn a_settlement_this_build_does_not_assign_is_not_reported_as_corruption() {
        // The same distinction `UnknownRoles` draws: the body is exactly the
        // shape a redirect takes, and it carries a value from a build that knows
        // more. Calling that corruption sends somebody to the wrong question.
        let mut body = redirect(Settlement::Settled).encode();
        let slot = body.get_mut(NODE_ID_LEN + 8).expect("the settlement byte");
        *slot = 9;
        assert!(matches!(
            Elsewhere::decode(&body),
            Err(Error::UnknownSettlement { byte: 9 })
        ));
    }

    #[test]
    fn a_zero_settlement_is_refused_because_a_zeroed_buffer_is_not_a_value() {
        let mut body = redirect(Settlement::Settled).encode();
        let slot = body.get_mut(NODE_ID_LEN + 8).expect("the settlement byte");
        *slot = 0;
        assert!(matches!(
            Elsewhere::decode(&body),
            Err(Error::UnknownSettlement { byte: 0 })
        ));
    }

    #[test]
    fn a_redirect_naming_nowhere_is_refused() {
        let mut empty = redirect(Settlement::Settled);
        empty.endpoint = String::new();
        assert!(matches!(
            Elsewhere::decode(&empty.encode()),
            Err(Error::Malformed)
        ));
    }
}
