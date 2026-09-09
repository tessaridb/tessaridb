//! The floor a series table answers above.
//!
//! A `DEFINE SERIES` table has a retention, and past it a record is not in the
//! answer. The removal is a separate act — so this module is what makes the
//! promise true, and a removal pass that lags, is throttled or never runs costs
//! storage and never an answer.
//!
//! # Why the floor is a record identity and not a predicate
//!
//! A series table's identity is [`tessari_types::IdentityKind::Uuid`], fixed by
//! the kind, and a UUID version 7 carries the millisecond it was minted in its
//! leading six bytes, big-endian. So "written more than a day ago" is a
//! **position in the key**: the walk opens at the floor instead of testing every
//! record it passes. That is the whole reason the retention is a kind rather
//! than a clause any table could carry — an ordinary table's counter identity
//! carries no time, and a rule over one of its datetime fields is re-tested per
//! record.
//!
//! # Why the floor is computed once per transaction
//!
//! A transaction reads at one snapshot; it also reads at one floor. Computing
//! it per call would let the boundary move underneath a walk that is still
//! running, so a scan begun at the turn of a second could answer with a record
//! its own later batch would have refused — inconsistency inside one read, with
//! nothing anywhere in an error state.

use tessari_encoding::decode_payload;
use tessari_types::{Duration, NamespaceId, RecordId, TableId};

use super::{RecordAddress, Transaction};
use crate::catalog::definition::{TableDefinition, TableKind};
use crate::catalog::system;
use crate::error::Result;

/// How many bytes of a UUID version 7 hold the millisecond.
const MILLISECOND_BYTES: usize = 6;

/// The bytes of a UUID.
const UUID_LEN: usize = 16;

impl Transaction<'_> {
    /// Whether this address names a record the table no longer answers with.
    ///
    /// Answers `false` for every table that is not a series, which is the
    /// overwhelming majority and costs one namespace comparison for them.
    pub(super) fn below_series_floor(&self, address: &RecordAddress) -> Result<bool> {
        let Some(floor) = self.series_floor(address.namespace, address.table)? else {
            return Ok(false);
        };
        Ok(is_below(&address.id, &floor))
    }

    /// The identity a series table answers from, when it is one.
    ///
    /// Cached per table for the transaction's life, so a walk over ten thousand
    /// records reads the catalog once.
    pub(crate) fn series_floor(
        &self,
        namespace: NamespaceId,
        table: TableId,
    ) -> Result<Option<RecordId>> {
        // The system namespace is excluded first and for a mechanical reason as
        // well as an obvious one: the definition below is read through `get`,
        // which asks this question, and a system table asking it about itself
        // would not terminate. No system table is a series.
        if namespace == system::SYSTEM_NAMESPACE {
            return Ok(None);
        }
        if let Some(held) = self.floors.borrow().get(&table) {
            return Ok(held.clone());
        }
        // The registry answers without touching the backend for every table
        // this process has declared or already looked up, which is what keeps a
        // point read on an ordinary table at one round trip.
        let retain = match self.store().series().known(table) {
            Some(retain) => retain,
            None => self.learn_kind(table)?,
        };
        let floor = retain.and_then(|retain| self.floor_at(retain));
        self.floors.borrow_mut().insert(table, floor.clone());
        Ok(floor)
    }

    /// Read the table's declaration once, and tell the registry what it says.
    fn learn_kind(&self, table: TableId) -> Result<Option<Duration>> {
        let entry = system::address(system::TABLES, RecordId::Int(i64::from(table.get())));
        let Some(bytes) = self.get_uncovered(&entry)? else {
            // Not found is **not** learned: a transaction whose snapshot
            // predates the table's creation would otherwise teach this process
            // that a series table is a plain one, permanently and silently.
            return Ok(None);
        };
        let definition = TableDefinition::from_value(&decode_payload(&bytes)?)?;
        self.store().series().learn(table, &definition.kind);
        Ok(match definition.kind {
            TableKind::Series(declared) => Some(declared.retain),
            _ => None,
        })
    }

    /// Where a retention puts the floor, as an identity.
    fn floor_at(&self, retain: Duration) -> Option<RecordId> {
        let seconds = retain.seconds();
        if seconds < 0 {
            return None;
        }
        // Widened rather than cast, and combined with `checked_*` so a
        // declaration nobody would write cannot wrap into a floor in the past.
        let retained = u64::try_from(seconds)
            .ok()
            .and_then(|seconds| seconds.checked_mul(1_000))
            .and_then(|millis| millis.checked_add(u64::from(retain.nanos()) / 1_000_000))?;
        Some(identity_at(self.reading_at().saturating_sub(retained)))
    }
}

/// The smallest UUID version 7 minted at or after `millis`.
///
/// Every byte below the millisecond is zero, which is what makes it a bound
/// rather than a value: a real identity minted in that millisecond carries the
/// version and variant bits, so it sorts after this one and inside the answer.
fn identity_at(millis: u64) -> RecordId {
    let mut bytes = [0_u8; UUID_LEN];
    let [_, _, t0, t1, t2, t3, t4, t5] = millis.to_be_bytes();
    bytes[0] = t0;
    bytes[1] = t1;
    bytes[2] = t2;
    bytes[3] = t3;
    bytes[4] = t4;
    bytes[5] = t5;
    RecordId::Uuid(bytes)
}

/// Whether an identity is past the floor.
///
/// An identity that is **not** a UUID is never below it. A series table names
/// its own records with UUID version 7, so this is a record somebody named by
/// hand, and a name carrying no time has no age for a floor to judge — hiding it
/// would be a wrong answer rather than a policy. Recorded as a question rather
/// than resolved by guessing: whether a series table should refuse such a write
/// where it happens, the way a vector store refuses a vector of the wrong width,
/// is a decision about the statement and not about this comparison.
fn is_below(id: &RecordId, floor: &RecordId) -> bool {
    match (id, floor) {
        (RecordId::Uuid(held), RecordId::Uuid(floor)) => {
            held[..MILLISECOND_BYTES] < floor[..MILLISECOND_BYTES]
        }
        _ => false,
    }
}
