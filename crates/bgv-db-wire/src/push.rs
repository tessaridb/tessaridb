//! Changes the node sends because they happened.
//!
//! # The reason this protocol has kinds
//!
//! Everything else here is a client asking and a node answering. This is the
//! node speaking first, and it is why frames carry a kind rather than being a
//! request followed by its reply. Tags 4 and 5 were reserved for it from the
//! start.
//!
//! # A slow subscriber cannot lose anything
//!
//! Not a promise made here — a property inherited. A subscription is a **durable
//! cursor over the log**, not a queue in front of it, so the buffer is the log:
//! ordered, durable and unbounded. A subscriber that reads slowly is *behind*,
//! never lossy, and the only way to lose a change is `skip_to`, which is always
//! the subscriber's own decision.
//!
//! That decides what happens to a client that stops reading altogether. Its
//! socket fills, this node's write blocks, and the write timeout ends the
//! connection saying so. **Nothing is buffered here and nothing is dropped**:
//! the client reconnects from the position it had, and the log still holds
//! everything. Buffering in the node instead would rebuild exactly the queue the
//! feed design removed, and its losses would be caused by memory pressure rather
//! than by a decision anyone made.
//!
//! # Subscribing takes the connection over
//!
//! A thread that is pushing cannot also be reading requests, and letting it do
//! both means multiplexing — a much larger protocol for a case nobody has. One
//! connection does one job; a client that wants both opens two, which the
//! per-connection session already implies.

use bgv_db::{Change, ChangeKind, Sequence, Value};
use bgv_db_encoding::{decode_payload, encode_payload};

use crate::error::{Error, Result};
use crate::frame::{put_bytes, put_text, put_u64, take_bytes, take_text, take_u64};

/// What a client asked to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
    /// Where to start — the first position to read, **inclusive**.
    ///
    /// A position in the log rather than "from now", so a client that was
    /// disconnected resumes from exactly where it stopped — which is the whole
    /// point of the cursor being a value it holds. A subscriber stores the
    /// `sequence` of the last change it handled and comes back with **one more
    /// than that**.
    ///
    /// Inclusive is what makes resuming say what it means: a client that came
    /// back with the position it had already handled would see it twice, and one
    /// that came back with the position it had *not* reached would be told it
    /// was caught up. So "everything from here on" is `committed_tail() + 1`,
    /// and `0` is everything the log still holds.
    pub from: u64,
    /// The table to watch, or every table in the session's database.
    pub table: Option<String>,
}

impl Follow {
    /// The body of a subscribe frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u64(&mut body, self.from);
        match &self.table {
            Some(name) => {
                body.push(1);
                put_text(&mut body, name);
            }
            None => body.push(0),
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body does not hold what it claims.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (from, at) = take_u64(body, 0)?;
        let flag = body.get(at).copied().ok_or(Error::Malformed)?;
        let table = match flag {
            0 => None,
            1 => Some(take_text(body, at.saturating_add(1))?.0),
            _ => return Err(Error::Malformed),
        };
        Ok(Self { from, table })
    }
}

/// The kind byte a change carries.
mod kind {
    pub(super) const WRITTEN: u8 = 0;
    pub(super) const REMOVED: u8 = 1;
}

/// One change, as it arrives.
///
/// The table is named rather than identified, for the same reason a records
/// answer carries names: an id is meaningless outside the process that minted
/// it, and the catalog is on the node.
#[derive(Debug, Clone, PartialEq)]
pub struct Happened {
    /// The commit this change was part of.
    ///
    /// Shared by every change of one commit, which is what lets a subscriber
    /// apply them as the unit they were written as — and what it stores to
    /// resume from.
    pub sequence: u64,
    /// The table, by name.
    pub table: String,
    /// The record's identity, as the store spells it.
    pub id: String,
    /// What became of it.
    pub became: Became,
}

/// What became of a record.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Became {
    /// It now holds this value.
    Written(Value),
    /// It is no longer there.
    Removed,
}

impl Happened {
    /// The same change with everything the subscriber may not read taken out.
    ///
    /// Through the session's own redactor rather than a copy of it: a second
    /// implementation of "what does this user see" is a second answer waiting to
    /// disagree with the first, and the disagreement would be silent.
    #[must_use]
    pub fn hiding(self, visible: &bgv_db::Visible) -> Self {
        match self.became {
            Became::Written(held) => Self {
                became: Became::Written(bgv_db::seen(held, visible)),
                ..self
            },
            // A removal carries no value, so there is nothing in it to hide —
            // and *that* a record was removed is what the table grant already
            // decided this subscriber may know.
            Became::Removed => self,
        }
    }

    /// The body of a change frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u64(&mut body, self.sequence);
        put_text(&mut body, &self.table);
        put_text(&mut body, &self.id);
        match &self.became {
            Became::Written(held) => {
                body.push(kind::WRITTEN);
                put_bytes(&mut body, encode_payload(held).as_slice());
            }
            Became::Removed => body.push(kind::REMOVED),
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`] when the body does not hold what it claims,
    /// and an encoding failure when the value cannot be decoded.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (sequence, at) = take_u64(body, 0)?;
        let (table, at) = take_text(body, at)?;
        let (id, at) = take_text(body, at)?;
        let became = match body.get(at).copied().ok_or(Error::Malformed)? {
            kind::WRITTEN => {
                let (bytes, _) = take_bytes(body, at.saturating_add(1))?;
                Became::Written(decode_payload(&bytes)?)
            }
            kind::REMOVED => Became::Removed,
            // Not skipped: a change whose kind this build cannot read is a
            // change it would deliver as the wrong thing, and a feed that
            // silently disagrees with the store is worse than one that stops.
            _ => return Err(Error::Malformed),
        };
        Ok(Self {
            sequence,
            table,
            id,
            became,
        })
    }
}

/// One change from the store, with its table named.
///
/// `None` when the table has been dropped: the change is real and its table has
/// no name to give, and inventing one would be worse than not sending it. The
/// subscription still advances past it, which is why this is a filter rather
/// than a failure.
pub(crate) fn named(change: &Change, table: Option<String>) -> Option<Happened> {
    Some(Happened {
        sequence: sequence_of(change.sequence),
        table: table?,
        id: change.id.to_string(),
        became: match &change.kind {
            ChangeKind::Written(held) => Became::Written(held.clone()),
            ChangeKind::Removed => Became::Removed,
        },
    })
}

/// A sequence as the number a client stores and sends back.
pub(crate) fn sequence_of(sequence: Sequence) -> u64 {
    sequence.get()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db::Value;

    use super::{Became, Follow, Happened};

    #[test]
    fn a_follow_round_trips_watching_one_table_and_all_of_them() {
        for table in [None, Some("users".to_owned())] {
            let held = Follow {
                from: 12_043,
                table,
            };
            assert_eq!(Follow::decode(&held.encode()).expect("a follow"), held);
        }
    }

    #[test]
    fn a_follow_body_that_promises_more_than_it_holds_is_refused() {
        let held = Follow {
            from: 1,
            table: Some("users".to_owned()),
        };
        let body = held.encode();
        for cut in 0..body.len() {
            assert!(
                Follow::decode(&body[..cut]).is_err(),
                "a cut at {cut} parsed"
            );
        }
    }

    #[test]
    fn a_change_round_trips_written_and_removed() {
        for became in [Became::Written(Value::from("ada")), Became::Removed] {
            let held = Happened {
                sequence: 7,
                table: "users".to_owned(),
                id: "1".to_owned(),
                became,
            };
            assert_eq!(Happened::decode(&held.encode()).expect("a change"), held);
        }
    }

    #[test]
    fn a_pushed_value_keeps_the_kind_json_would_have_flattened() {
        // A subscriber applying changes has exactly the problem a console has:
        // a decimal it has to decide about is a decimal it will get wrong.
        let held = Happened {
            sequence: 1,
            table: "prices".to_owned(),
            id: "1".to_owned(),
            became: Became::Written(Value::Number(bgv_db::Number::Decimal(
                rust_decimal::Decimal::try_from(12.34_f64).expect("a decimal"),
            ))),
        };
        match Happened::decode(&held.encode()).expect("a change").became {
            Became::Written(Value::Number(bgv_db::Number::Decimal(read))) => {
                assert_eq!(read.to_string(), "12.34");
            }
            other => panic!("a decimal came back as {other:?}"),
        }
    }

    #[test]
    fn a_change_body_that_promises_more_than_it_holds_is_refused() {
        let held = Happened {
            sequence: 7,
            table: "users".to_owned(),
            id: "1".to_owned(),
            became: Became::Written(Value::from("ada")),
        };
        let body = held.encode();
        for cut in 0..body.len() {
            assert!(
                Happened::decode(&body[..cut]).is_err(),
                "a cut at {cut} parsed"
            );
        }
    }
}
