//! The instant each message of a topic was appended (ADR-0133 D3).
//!
//! The appending commit writes the topic's clock — the newest instant it has
//! given, never lower than the one before — as an ordinary system record in
//! its own log record. Applying the record files that instant beside each new
//! position, so a replica derives exactly the instants the writer gave without
//! consulting a clock of its own, and a leader whose clock runs behind the one
//! before it keeps the topic's instants from going back.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    CausalStamp, LogRecord, Mutation, NODE_ID_LEN, RecordValue, StampedValue, decode_payload,
    encode_payload,
};
use tessari_types::{Number, RecordId, Value};

use super::Topic;
use crate::catalog::system;
use crate::error::Result;
use crate::transaction::{RecordAddress, Transaction};

/// The clock record's identity for one topic.
fn row((namespace, database, table): Topic) -> RecordId {
    RecordId::Text(format!(
        "{}/{}/{}",
        namespace.get(),
        database.get(),
        table.get()
    ))
}

fn address(topic: Topic) -> RecordAddress {
    system::address(system::TOPIC_CLOCKS, row(topic))
}

/// The instant a clock record holds, or `None` for one that holds none.
fn instant_in(value: &RecordValue) -> Result<Option<u64>> {
    let RecordValue::Present(payload) = value else {
        return Ok(None);
    };
    Ok(match decode_payload(payload)? {
        Value::Number(Number::Integer(held)) => u64::try_from(held).ok(),
        _ => None,
    })
}

/// Add to `mutations` one clock write per topic in `appended`, each holding
/// the later of `now` and the instant the topic last gave.
///
/// # Errors
///
/// A backend failure, or a clock record that cannot be decoded.
pub(super) async fn stamp(
    view: &Transaction<'_>,
    appended: &BTreeSet<Topic>,
    now: u64,
    node: [u8; NODE_ID_LEN],
    mutations: &mut Vec<Mutation>,
) -> Result<()> {
    for topic in appended {
        let address = address(*topic);
        let held = view.read_newest_stamped(&address).await?;
        let previous = match &held {
            Some((_, stamped)) => instant_in(stamped.value())?,
            None => None,
        };
        let instant = previous.map_or(now, |previous| previous.max(now));
        let mut stamp = held.map_or_else(CausalStamp::new, |(_, stamped)| stamped.stamp().clone());
        stamp.advance(node);
        mutations.push(Mutation {
            namespace: system::SYSTEM_NAMESPACE,
            database: system::SYSTEM_DATABASE,
            table: system::TOPIC_CLOCKS,
            id: address.id.clone(),
            shard: None,
            value: StampedValue::stamped(
                stamp,
                RecordValue::Present(
                    encode_payload(&Value::from(i64::try_from(instant).unwrap_or(i64::MAX)))
                        .into_bytes(),
                ),
            ),
        });
    }
    Ok(())
}

/// The instant `record` gives each topic it appends to.
///
/// A record written before topics kept the instant gives none, and its
/// positions are filed without one.
///
/// # Errors
///
/// A clock record that cannot be decoded.
pub(super) fn given(record: &LogRecord, topics: &BTreeSet<Topic>) -> Result<BTreeMap<Topic, u64>> {
    let rows: BTreeMap<RecordId, Topic> =
        topics.iter().map(|topic| (row(*topic), *topic)).collect();
    let mut given = BTreeMap::new();
    for mutation in record.mutations() {
        if mutation.namespace != system::SYSTEM_NAMESPACE
            || mutation.database != system::SYSTEM_DATABASE
            || mutation.table != system::TOPIC_CLOCKS
        {
            continue;
        }
        if let (Some(topic), Some(instant)) =
            (rows.get(&mutation.id), instant_in(mutation.value.value())?)
        {
            given.insert(*topic, instant);
        }
    }
    Ok(given)
}
