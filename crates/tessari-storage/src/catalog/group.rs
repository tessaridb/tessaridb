//! Consumer groups on a topic (G042, ADR-0086).
//!
//! A group is the readers who read a topic under one name and acknowledge each
//! message they are given. Everything a group is — its declaration, the last
//! position it has handed out, and the messages it holds in flight with their
//! deadlines — is one record in the system tenancy, written through the reader's
//! own transaction as a topic position is.
//!
//! # Why one record and not one per message in flight
//!
//! A record per message would let two members acknowledging different messages
//! commit without meeting, and would make every read walk the whole system table
//! to find the group's messages, because a system table is read whole. One
//! record bounded by the group's `IN FLIGHT` width costs the members a turn each
//! on it and nothing else, and that turn is the one a group of readers under one
//! name already takes on a topic position (G037).
//!
//! # A deadline is a written instant
//!
//! As a queue's hold is: nothing sweeps a group. A message whose deadline has
//! passed is simply handed out again by the next read, and a follower or a
//! restarted node reaches the same answer from the same record.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{
    DatabaseId, Datetime, Duration, NamespaceId, Number, RecordId, TableId, Value,
};

use super::{Catalog, system};
use crate::error::{Error, Result};

const ENTITY: &str = "topic group";
const FIELD_NAME: &str = "name";
const FIELD_DEADLINE: &str = "deadline";
const FIELD_DELIVERIES: &str = "deliveries";
const FIELD_WIDTH: &str = "in_flight";
const FIELD_DEAD_LETTER: &str = "dead_letter";
const FIELD_CURSOR: &str = "cursor";
const FIELD_FLIGHT: &str = "flight";
const FIELD_POSITION: &str = "position";
const FIELD_UNTIL: &str = "until";
const FIELD_REDELIVERED: &str = "redelivered";
const FIELD_DEAD_LETTERED: &str = "dead_lettered";

/// What `DEFINE GROUP` declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupDeclaration {
    /// How long a member has to acknowledge a message before it is handed out
    /// again.
    pub deadline: Duration,
    /// After this many deliveries a message is dead-lettered; `None` is never.
    pub deliveries: Option<u64>,
    /// The most messages held unacknowledged at once. Never zero.
    pub width: u64,
    /// The topic dead-lettered messages are appended to, in the same database.
    pub dead_letter: Option<TableId>,
}

/// One message a group has handed out and not yet had acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    /// Its position in the topic.
    pub position: u64,
    /// When it may be handed out again.
    pub until: Datetime,
    /// How many times it has been handed out.
    pub deliveries: u64,
}

/// A group as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupState {
    /// What it was declared as.
    pub declaration: GroupDeclaration,
    /// The last position it has handed out for the first time.
    pub cursor: u64,
    /// What it holds unacknowledged, in position order.
    pub flight: Vec<InFlight>,
    /// Deliveries after the first, over the group's life.
    pub redelivered: u64,
    /// Messages given up on after their last delivery, over the group's life.
    pub dead_lettered: u64,
}

fn row(namespace: NamespaceId, database: DatabaseId, table: TableId, name: &str) -> RecordId {
    RecordId::Text(format!(
        "{}/{}/{}/{name}",
        namespace.get(),
        database.get(),
        table.get()
    ))
}

fn topic_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> String {
    format!("{}/{}/{}/", namespace.get(), database.get(), table.get())
}

fn integer(count: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn malformed(field: &'static str, found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found,
    }
}

fn whole(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<u64>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::Number(Number::Integer(held))) => u64::try_from(*held)
            .map(Some)
            .map_err(|_| malformed(field, "negative")),
        Some(_) => Err(malformed(field, "not a whole number")),
    }
}

fn required(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<u64> {
    whole(fields, field)?.ok_or_else(|| malformed(field, "missing"))
}

fn object<'a>(value: &'a Value, field: &'static str) -> Result<&'a BTreeMap<String, Value>> {
    match value {
        Value::Object(fields) => Ok(fields),
        _ => Err(malformed(field, "not an object")),
    }
}

impl InFlight {
    fn to_value(self) -> Value {
        Value::Object(BTreeMap::from([
            (FIELD_POSITION.to_owned(), integer(self.position)),
            (FIELD_UNTIL.to_owned(), Value::Datetime(self.until)),
            (FIELD_DELIVERIES.to_owned(), integer(self.deliveries)),
        ]))
    }

    fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, FIELD_FLIGHT)?;
        let until = match fields.get(FIELD_UNTIL) {
            Some(Value::Datetime(until)) => *until,
            _ => return Err(malformed(FIELD_UNTIL, "not a datetime")),
        };
        Ok(Self {
            position: required(fields, FIELD_POSITION)?,
            until,
            deliveries: required(fields, FIELD_DELIVERIES)?,
        })
    }
}

impl GroupState {
    /// A group just declared: nothing handed out after `cursor`, nothing held.
    #[must_use]
    pub const fn new(declaration: GroupDeclaration, cursor: u64) -> Self {
        Self {
            declaration,
            cursor,
            flight: Vec::new(),
            redelivered: 0,
            dead_lettered: 0,
        }
    }

    /// The last position every message up to which is acknowledged — the
    /// group's committed position, from which its lag is counted.
    #[must_use]
    pub fn committed(&self) -> u64 {
        self.flight
            .first()
            .map_or(self.cursor, |held| held.position.saturating_sub(1))
    }

    fn to_value(&self, name: &str) -> Value {
        let declared = self.declaration;
        let mut fields = BTreeMap::from([
            (FIELD_NAME.to_owned(), Value::from(name)),
            (
                FIELD_DEADLINE.to_owned(),
                Value::Duration(declared.deadline),
            ),
            (FIELD_WIDTH.to_owned(), integer(declared.width)),
            (FIELD_CURSOR.to_owned(), integer(self.cursor)),
            (
                FIELD_FLIGHT.to_owned(),
                Value::Array(self.flight.iter().map(|held| held.to_value()).collect()),
            ),
            (FIELD_REDELIVERED.to_owned(), integer(self.redelivered)),
            (FIELD_DEAD_LETTERED.to_owned(), integer(self.dead_lettered)),
        ]);
        if let Some(deliveries) = declared.deliveries {
            fields.insert(FIELD_DELIVERIES.to_owned(), integer(deliveries));
        }
        if let Some(dead_letter) = declared.dead_letter {
            fields.insert(
                FIELD_DEAD_LETTER.to_owned(),
                integer(u64::from(dead_letter.get())),
            );
        }
        Value::Object(fields)
    }

    fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, FIELD_NAME)?;
        let deadline = match fields.get(FIELD_DEADLINE) {
            Some(Value::Duration(deadline)) => *deadline,
            _ => return Err(malformed(FIELD_DEADLINE, "not a duration")),
        };
        let dead_letter = match whole(fields, FIELD_DEAD_LETTER)? {
            Some(id) => Some(TableId::new(
                u32::try_from(id).map_err(|_| malformed(FIELD_DEAD_LETTER, "out of range"))?,
            )),
            None => None,
        };
        let flight = match fields.get(FIELD_FLIGHT) {
            Some(Value::Array(held)) => held
                .iter()
                .map(InFlight::from_value)
                .collect::<Result<Vec<_>>>()?,
            _ => return Err(malformed(FIELD_FLIGHT, "not an array")),
        };
        Ok(Self {
            declaration: GroupDeclaration {
                deadline,
                deliveries: whole(fields, FIELD_DELIVERIES)?,
                width: required(fields, FIELD_WIDTH)?,
                dead_letter,
            },
            cursor: required(fields, FIELD_CURSOR)?,
            flight,
            redelivered: required(fields, FIELD_REDELIVERED)?,
            dead_lettered: required(fields, FIELD_DEAD_LETTERED)?,
        })
    }
}

impl Catalog<'_, '_> {
    /// The group reading this topic under `name`, or `None` when there is
    /// none.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a group.
    pub fn topic_group(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        name: &str,
    ) -> Result<Option<GroupState>> {
        let address = system::address(system::TOPIC_GROUPS, row(namespace, database, table, name));
        match self.transaction.get(&address)? {
            Some(payload) => Ok(Some(GroupState::from_value(&decode_payload(&payload)?)?)),
            None => Ok(None),
        }
    }

    /// Write the group as it now stands.
    pub fn put_topic_group(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        name: &str,
        state: &GroupState,
    ) {
        self.transaction.put(
            system::address(system::TOPIC_GROUPS, row(namespace, database, table, name)),
            encode_payload(&state.to_value(name)).into_bytes(),
        );
    }

    /// Forget the group and everything it holds in flight.
    pub fn drop_topic_group(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        name: &str,
    ) {
        self.transaction.delete(system::address(
            system::TOPIC_GROUPS,
            row(namespace, database, table, name),
        ));
    }

    /// Every group reading this topic, by name.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a group.
    pub fn topic_groups(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<(String, GroupState)>> {
        let prefix = topic_prefix(namespace, database, table);
        let mut found = Vec::new();
        for (id, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::TOPIC_GROUPS,
        )? {
            let RecordId::Text(key) = &id else {
                continue;
            };
            if let Some(name) = key.strip_prefix(&prefix) {
                found.push((
                    name.to_owned(),
                    GroupState::from_value(&decode_payload(&payload)?)?,
                ));
            }
        }
        Ok(found)
    }
}
