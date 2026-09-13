//! Which node the log last showed leading a range, and under which leadership.
//!
//! # Why this is in the log at all
//!
//! Until this existed, a granted leadership lived in one place: the winner's own
//! memory. [`crate::Store::hold`] takes the lease and sets an `Option<Epoch>`,
//! and that is the whole record — it dies with the process and reaches a peer
//! only through a greeting. A [`tessari_encoding::LogRecord`] carries the epoch
//! it was written under but never the node that wrote it, so the log could not
//! name a leader even in principle.
//!
//! That is why the one production answer to *who leads* — `tessari_wire::upstream`
//! — reads the greeting directory, and why ADR-0065 chose it. **ADR-0065 did not
//! prefer the network over the log; it chose the only source that existed.**
//!
//! A row here is what puts the fact in the log. It is an ordinary record in the
//! system tenancy (ADR-0009), so it is already a log record, already ordered by
//! the lease-holding leader, and already replicates through the same apply path:
//! no new record kind, no encoding change, nothing new on the wire.
//!
//! # What it is not
//!
//! It is **not** configuration, and the difference from the peer row's `roles`
//! matters. A `roles` field is an operator's statement of what a node *should*
//! be, which is exactly why ADR-0065 found it could not name a leader. This row
//! is written **by the winner, at the moment it wins, under its own epoch**,
//! through the log a majority ordered.
//!
//! It is also **not a promise about now**. Like any record of a runtime fact it
//! can outlive the lease: the leader dies and the row stands. So the answer is
//! *as of epoch E, node N led range R* — a claim about the log, carrying the
//! epoch it was decided under, refusable by a client that has already been told
//! about a newer one (`tessari_wire::Error::StaleRedirect`).
//!
//! # Why it carries no expiry, no heartbeat and no freshness field
//!
//! Each would be a second mechanism for a job the epoch already does. A reader
//! that must know whether a leadership is still live asks the node, and the
//! answer it gets back is ordered against the epoch it already holds.
//!
//! # One row per range, and the key is a handle rather than a datum
//!
//! A new leadership for a range replaces the previous row instead of appending
//! to a history, so the record is keyed by a string derived from the range. That
//! derivation is [`LeadershipDefinition::key`] and it has exactly one caller
//! shape: nothing ever reads a range back **out** of a key. The range is read
//! from the value, so there is one authoritative spelling of it on disk and no
//! second reading that could disagree with the first.

use std::collections::BTreeMap;

use tessari_encoding::{NODE_ID_LEN, decode_payload, encode_payload};
use tessari_types::{Epoch, Number, RecordId, Value};

use super::authority::Reach;
use super::definition::{count_of, object};
use super::{Catalog, system};
use crate::error::{Error, Result};

const FIELD_RANGE: &str = "range";
const FIELD_NODE: &str = "node";
const FIELD_EPOCH: &str = "epoch";

const ENTITY: &str = "leadership";

/// A leadership the log records: a range, the node that took it, and the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeadershipDefinition {
    /// The range this leadership covers.
    ///
    /// Only [`Reach::Store`] is written today, because today the lease is one
    /// lease over the whole store. The field is a range from the first version
    /// anyway: G025's S6.2 makes the epoch per-range so that true multi-master
    /// on one dataset stays reachable, and a column added later is a migration
    /// where a column keyed correctly now is one field.
    pub range: Reach,
    /// Who took it.
    pub node: [u8; NODE_ID_LEN],
    /// The leadership it was taken under.
    ///
    /// This is what makes the answer checkable rather than merely available: a
    /// reader holding a newer epoch knows this row describes an arrangement that
    /// has been superseded, instead of following it and looping.
    pub epoch: Epoch,
}

impl LeadershipDefinition {
    /// The record this range's leadership is stored under.
    ///
    /// A stable string per range, so writing a new leadership for a range
    /// replaces the row rather than adding to a history nobody reads. It is a
    /// **handle**: the range itself is read from the value, never from here.
    #[must_use]
    pub fn key(range: Reach) -> RecordId {
        let (namespace, database) = range.parts();
        RecordId::from(match (namespace, database) {
            (None, _) => "store".to_owned(),
            (Some(namespace), None) => format!("ns:{}", namespace.get()),
            (Some(namespace), Some(database)) => {
                format!("db:{}:{}", namespace.get(), database.get())
            }
        })
    }

    /// The value written to the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the epoch is past `i64::MAX`,
    /// which is the widest integer a stored value holds. It is refused rather
    /// than truncated: an epoch that wrapped would compare as older than the one
    /// before it, and every ordering rule in the cluster reads that comparison.
    /// The bound is unreachable in practice — the epoch advances once per
    /// leadership change — which is why refusing it costs nothing.
    pub fn to_value(&self) -> Result<Value> {
        let epoch = i64::try_from(self.epoch.get()).map_err(|_| Error::CatalogMalformed {
            entity: ENTITY,
            field: FIELD_EPOCH,
            found: "number",
        })?;
        Ok(Value::Object(BTreeMap::from([
            (FIELD_RANGE.to_owned(), self.range.to_value()),
            (FIELD_NODE.to_owned(), Value::Uuid(self.node)),
            (
                FIELD_EPOCH.to_owned(),
                Value::Number(Number::Integer(epoch)),
            ),
        ])))
    }

    /// Read a definition back.
    ///
    /// Every field is required. None of the three was added after the fact, so
    /// there is no older row for an absent-reads-as-default rule to rescue — and
    /// a leadership missing any one of them is not a leadership this build can
    /// route on.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: fields.get(field).map_or("none", Value::type_name),
        };
        let Some(range) = fields.get(FIELD_RANGE) else {
            return Err(malformed(FIELD_RANGE));
        };
        let Some(Value::Uuid(node)) = fields.get(FIELD_NODE) else {
            return Err(malformed(FIELD_NODE));
        };
        let Some(epoch) = fields.get(FIELD_EPOCH) else {
            return Err(malformed(FIELD_EPOCH));
        };
        Ok(Self {
            range: Reach::from_value(range, ENTITY, FIELD_RANGE)?,
            node: *node,
            epoch: Epoch::new(count_of(epoch, ENTITY, FIELD_EPOCH)?),
        })
    }
}

impl Catalog<'_, '_> {
    /// Record that `node` took the leadership of `range` at `epoch`.
    ///
    /// # This is written on a change and never on a renewal
    ///
    /// A lease is renewed every round for as long as a node keeps leading, and a
    /// row written per renewal would be a log record every few seconds forever —
    /// on a log that would then never quiesce, in a store whose followers pay to
    /// apply each one. The caller writes this only when the epoch it holds is
    /// not the epoch it held a moment ago, which is the same guard the binary
    /// already uses before installing a granted lease.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the epoch cannot be stored; see
    /// [`LeadershipDefinition::to_value`].
    pub fn record_leadership(
        &mut self,
        range: Reach,
        node: [u8; NODE_ID_LEN],
        epoch: Epoch,
    ) -> Result<LeadershipDefinition> {
        let definition = LeadershipDefinition { range, node, epoch };
        self.transaction.put(
            system::address(system::LEADERSHIPS, LeadershipDefinition::key(range)),
            encode_payload(&definition.to_value()?).into_bytes(),
        );
        Ok(definition)
    }

    /// Every leadership this node has applied, in range order.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn leaderships(&self) -> Result<Vec<LeadershipDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::LEADERSHIPS,
        )? {
            found.push(LeadershipDefinition::from_value(&decode_payload(
                &payload,
            )?)?);
        }
        found.sort_by_key(|held| ordered(held.range));
        Ok(found)
    }

    /// Who the log says leads `range`, and under which leadership.
    ///
    /// **No network call, no greeting, no peer.** The answer comes from a row
    /// this node holds because it applied the log record that created it, which
    /// is the whole of what the criterion asks: a node partitioned from every
    /// other still answers, from what it had already applied.
    ///
    /// # A leadership over a wider range covers the ranges inside it
    ///
    /// Asked about one database, this answers the leadership of that database if
    /// one was recorded, and otherwise the leadership of the namespace above it,
    /// and otherwise the store's. That is [`Reach::contains`] — the one
    /// implication in the tenancy model — and it is what makes the question
    /// answerable today, when the only row ever written is the store's.
    ///
    /// The **most specific** covering row wins. With one lease over the store
    /// there is never a choice to make; when S6.2 splits the epoch by range
    /// there will be, and a rule that picked the widest would answer a database's
    /// question with the store's leadership while the database had its own.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub fn leader_of(&self, range: Reach) -> Result<Option<LeadershipDefinition>> {
        Ok(self
            .leaderships()?
            .into_iter()
            .filter(|held| held.range.contains(range))
            .max_by_key(|held| ordered(held.range)))
    }
}

/// A range as a sort key: wider first, and more specific later.
///
/// One function for both the listing order and the most-specific choice, so the
/// two cannot disagree about which of two ranges is narrower.
fn ordered(range: Reach) -> (u8, u32, u32) {
    match range {
        Reach::Store => (0, 0, 0),
        Reach::Namespace(namespace) => (1, namespace.get(), 0),
        Reach::Database(namespace, database) => (2, namespace.get(), database.get()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::{ENTITY, FIELD_EPOCH, FIELD_NODE, FIELD_RANGE, LeadershipDefinition};
    use std::collections::BTreeMap;
    use tessari_encoding::NODE_ID_LEN;
    use tessari_types::{DatabaseId, Epoch, NamespaceId, Number, Value};

    use crate::catalog::authority::Reach;
    use crate::error::Error;

    const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];

    fn held(range: Reach, epoch: u64) -> LeadershipDefinition {
        LeadershipDefinition {
            range,
            node: ONE,
            epoch: Epoch::new(epoch),
        }
    }

    #[test]
    fn a_leadership_round_trips_with_its_range_and_epoch_unchanged() {
        for range in [
            Reach::Store,
            Reach::Namespace(NamespaceId::new(3)),
            Reach::Database(NamespaceId::new(3), DatabaseId::new(7)),
        ] {
            let written = held(range, 9);
            let read = LeadershipDefinition::from_value(&written.to_value().unwrap()).unwrap();
            assert_eq!(read, written, "{range:?} did not survive the round trip");
        }
    }

    #[test]
    fn each_range_is_stored_under_its_own_record_and_the_same_range_under_the_same_one() {
        let store = LeadershipDefinition::key(Reach::Store);
        let namespace = LeadershipDefinition::key(Reach::Namespace(NamespaceId::new(3)));
        let database =
            LeadershipDefinition::key(Reach::Database(NamespaceId::new(3), DatabaseId::new(7)));
        assert_ne!(store, namespace);
        assert_ne!(namespace, database);
        assert_ne!(store, database);
        // The key is what makes a new leadership REPLACE the previous one rather
        // than append to a history nobody reads, so the same range must land on
        // the same record however many times it is asked for.
        assert_eq!(store, LeadershipDefinition::key(Reach::Store));
        assert_eq!(
            database,
            LeadershipDefinition::key(Reach::Database(NamespaceId::new(3), DatabaseId::new(7)))
        );
    }

    #[test]
    fn a_row_missing_any_of_the_three_fields_is_refused_rather_than_defaulted() {
        let whole = held(Reach::Store, 4).to_value().unwrap();
        let Value::Object(fields) = whole else {
            panic!("a definition encodes as an object");
        };
        for missing in [FIELD_RANGE, FIELD_NODE, FIELD_EPOCH] {
            let mut without = fields.clone();
            without.remove(missing);
            let refused = LeadershipDefinition::from_value(&Value::Object(without)).unwrap_err();
            // Not a default and not a silent absence: a leadership missing its
            // range would route somewhere, and one missing its epoch could not
            // be compared against a newer one — the whole of what makes the
            // answer checkable.
            assert!(
                matches!(
                    refused,
                    Error::CatalogMalformed { entity, field, .. }
                        if entity == ENTITY && field == missing
                ),
                "{missing} was not refused by name: {refused:?}"
            );
        }
    }

    #[test]
    fn an_epoch_stored_as_something_else_is_refused_and_not_read_as_zero() {
        let mut fields = BTreeMap::from([
            (FIELD_RANGE.to_owned(), Reach::Store.to_value()),
            (FIELD_NODE.to_owned(), Value::Uuid(ONE)),
            (FIELD_EPOCH.to_owned(), Value::from("seven")),
        ]);
        let refused = LeadershipDefinition::from_value(&Value::Object(fields.clone())).unwrap_err();
        assert!(matches!(refused, Error::CatalogMalformed { field, .. } if field == FIELD_EPOCH));

        // A negative epoch is the sharper case. Nothing writes one, so one being
        // there means this row is not what this build takes it for — and reading
        // it back as an enormous number would make it beat every real leadership
        // in the only comparison that orders them.
        fields.insert(
            FIELD_EPOCH.to_owned(),
            Value::Number(Number::Integer(-1_i64)),
        );
        let refused = LeadershipDefinition::from_value(&Value::Object(fields)).unwrap_err();
        assert!(matches!(refused, Error::CatalogMalformed { field, .. } if field == FIELD_EPOCH));
    }

    #[test]
    fn an_epoch_past_what_a_stored_value_holds_is_refused_rather_than_wrapped() {
        let past = LeadershipDefinition {
            range: Reach::Store,
            node: ONE,
            epoch: Epoch::new(u64::MAX),
        };
        // Unreachable in practice — the epoch advances once per leadership
        // change — which is exactly why refusing costs nothing. Truncating would
        // not: an epoch that wrapped compares as OLDER than the one before it,
        // and every ordering rule in the cluster reads that comparison.
        assert!(matches!(
            past.to_value().unwrap_err(),
            Error::CatalogMalformed { field, .. } if field == FIELD_EPOCH
        ));
    }
}
