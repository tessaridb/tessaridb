//! A series ordered by event time: each record's identity is minted from its
//! own time field rather than from the clock (ADR-0088 §1).
//!
//! # Why the identity and not an index
//!
//! Everything that makes a series a series is already a property of its key:
//! the retention floor is a UUID bound, a scan walks key order, and an index on
//! a field lists its entries in identity order. So moving the time the key
//! carries — from arrival to the event — moves all of them at once, and a late
//! event lands in its place without a second structure to keep in step.
//!
//! # Why one check guards every write
//!
//! [`hold_event_time`] runs where every caller-driven write passes, and asks one
//! question: does this record's identity carry this record's time? A hand-named
//! identity fails it, and so does an update that moved the time field, and the
//! two are told apart only by whether the record was already there.

use tessari_ql::Span;
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{Datetime, NamespaceId, RecordId, TableId, Value};

use crate::error::{Error, Result};
use crate::generate;

/// The largest millisecond count a UUID version 7 carries: 48 bits.
const MAX_MILLIS: u64 = (1 << 48) - 1;

/// Nanoseconds in a millisecond.
const NANOS_PER_MILLI: u32 = 1_000_000;

/// The sub-millisecond fraction's resolution: twelve bits.
const FRACTIONS_PER_MILLI: u64 = 4096;

/// An event time as a UUID version 7 carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// Milliseconds since the epoch.
    millis: u64,
    /// The part of the millisecond below it, in 4096ths.
    fraction: u16,
}

impl Stamp {
    /// The stamp for `at`, or `None` before 1970 or past the 48-bit range.
    fn of(at: Datetime) -> Option<Self> {
        let seconds = u64::try_from(at.seconds()).ok()?;
        let millis = seconds
            .checked_mul(1_000)?
            .checked_add(u64::from(at.nanos() / NANOS_PER_MILLI))?;
        if millis > MAX_MILLIS {
            return None;
        }
        let within = u64::from(at.nanos() % NANOS_PER_MILLI);
        let fraction = within
            .checked_mul(FRACTIONS_PER_MILLI)?
            .checked_div(u64::from(NANOS_PER_MILLI))?;
        Some(Self {
            millis,
            fraction: u16::try_from(fraction).ok()?,
        })
    }

    /// Whether a UUID's leading 60 time bits are this stamp.
    fn carried_by(self, bytes: &[u8; 16]) -> bool {
        let [_, _, t0, t1, t2, t3, t4, t5] = self.millis.to_be_bytes();
        let [high, low] = self.fraction.to_be_bytes();
        bytes[..6] == [t0, t1, t2, t3, t4, t5] && bytes[6] & 0x0f == high && bytes[7] == low
    }
}

/// The name a table is written back as, for a refusal.
fn named(transaction: &mut Transaction<'_>, table: TableId) -> Result<String> {
    Ok(Catalog::new(transaction)
        .table(table)?
        .map_or_else(|| table.to_string(), |found| found.name))
}

/// The event time `payload` carries in `field`, as a stamp.
fn stamp_of(
    transaction: &mut Transaction<'_>,
    table: TableId,
    field: &str,
    payload: &Value,
    span: Span,
) -> Result<(Stamp, Datetime)> {
    let found = match payload {
        Value::Object(fields) => fields.get(field),
        _ => None,
    };
    let Some(Value::Datetime(at)) = found else {
        return Err(Error::SeriesTimeMissing {
            table: named(transaction, table)?,
            field: field.to_owned(),
            found: found.map_or("absent", Value::type_name),
            span,
        });
    };
    let Some(stamp) = Stamp::of(*at) else {
        return Err(Error::SeriesTimeOutOfRange {
            table: named(transaction, table)?,
            instant: at.to_string(),
            span,
        });
    };
    Ok((stamp, *at))
}

/// The identity the store gives a record of a series ordered by event time, or
/// `None` when the table is not one.
///
/// # Errors
///
/// [`Error::SeriesTimeMissing`] and [`Error::SeriesTimeOutOfRange`] for a record
/// whose time field cannot order it, and [`Error::IdentityUnavailable`] when the
/// randomness below the time cannot be read.
pub(crate) fn event_identity(
    transaction: &mut Transaction<'_>,
    namespace: NamespaceId,
    table: TableId,
    payload: &Value,
    span: Span,
) -> Result<Option<RecordId>> {
    let Some(field) = transaction.series_time(namespace, table)? else {
        return Ok(None);
    };
    let (stamp, _) = stamp_of(transaction, table, &field, payload, span)?;
    Ok(Some(RecordId::Uuid(generate::uuid_v7_at(
        stamp.millis,
        stamp.fraction,
        span,
    )?)))
}

/// Refuse a write to a series ordered by event time whose identity does not
/// carry the record's time, or whose time is already past the floor.
///
/// Answers `Ok` at the cost of one registry lookup for every table that is not
/// such a series.
///
/// # Errors
///
/// [`Error::SeriesIdentityDerived`] for a hand-named identity,
/// [`Error::SeriesTimeFixed`] for an update that moved the time field,
/// [`Error::BelowSeriesFloor`] for an event the table would never answer, and
/// the refusals of [`event_identity`] for a time field that cannot order it.
pub(crate) fn hold_event_time(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    payload: &Value,
    span: Span,
) -> Result<()> {
    let Some(field) = transaction.series_time(address.namespace, address.table)? else {
        return Ok(());
    };
    let (stamp, at) = stamp_of(transaction, address.table, &field, payload, span)?;
    let carried = matches!(&address.id, RecordId::Uuid(bytes) if stamp.carried_by(bytes));
    if !carried {
        let table = named(transaction, address.table)?;
        return Err(if transaction.get(address)?.is_some() {
            Error::SeriesTimeFixed { table, field, span }
        } else {
            Error::SeriesIdentityDerived { table, field, span }
        });
    }
    if transaction.below_floor(address)? {
        return Err(Error::BelowSeriesFloor {
            table: named(transaction, address.table)?,
            instant: at.to_string(),
            span,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::Stamp;
    use tessari_types::Datetime;

    #[test]
    fn a_stamp_orders_below_the_millisecond_and_refuses_before_1970() {
        let early = Stamp::of(Datetime::new(1_700_000_000, 1_000).unwrap()).unwrap();
        let later = Stamp::of(Datetime::new(1_700_000_000, 500_000).unwrap()).unwrap();
        assert_eq!(early.millis, later.millis);
        assert!(early.fraction < later.fraction);
        assert_eq!(later.fraction, 2048);
        assert!(Stamp::of(Datetime::new(-1, 0).unwrap()).is_none());
    }
}
