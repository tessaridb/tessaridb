//! Where each reader of a topic has reached (G037).
//!
//! One record per topic and reader name, in the system tenancy, holding the
//! last position the reader was given. Written through the reader's own
//! transaction: a position that moved while the reader's writes did not — or
//! the other way round — is the failure a broker beside a database has to
//! approximate with deduplication, and here it cannot happen because the two
//! are one commit.

use std::collections::BTreeMap;

use tessari_encoding::decode_payload;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use super::{Catalog, system};
use crate::error::{Error, Result};

const ENTITY: &str = "topic position";
const FIELD_TOPIC: &str = "topic";
const FIELD_CONSUMER: &str = "consumer";
const FIELD_POSITION: &str = "position";

/// The row a reader's position is kept in. The namespace and database are part
/// of it because a table id is unique only within its database.
fn row(namespace: NamespaceId, database: DatabaseId, table: TableId, consumer: &str) -> RecordId {
    RecordId::Text(format!(
        "{}/{}/{}/{consumer}",
        namespace.get(),
        database.get(),
        table.get()
    ))
}

fn topic_prefix(namespace: NamespaceId, database: DatabaseId, table: TableId) -> String {
    format!("{}/{}/{}/", namespace.get(), database.get(), table.get())
}

fn position_in(value: &Value) -> Result<u64> {
    let Value::Object(fields) = value else {
        return Err(malformed(FIELD_POSITION, "not an object"));
    };
    match fields.get(FIELD_POSITION) {
        Some(Value::Number(tessari_types::Number::Integer(held))) => {
            u64::try_from(*held).map_err(|_| malformed(FIELD_POSITION, "negative"))
        }
        _ => Err(malformed(FIELD_POSITION, "not a whole number")),
    }
}

fn malformed(field: &'static str, found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found,
    }
}

impl Catalog<'_, '_> {
    /// The last position `consumer` was given in this topic, or `None` when it
    /// has never read it.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a position.
    pub fn topic_position(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        consumer: &str,
    ) -> Result<Option<u64>> {
        let address = system::address(
            system::TOPIC_POSITIONS,
            row(namespace, database, table, consumer),
        );
        match self.transaction.get(&address)? {
            Some(payload) => Ok(Some(position_in(&decode_payload(&payload)?)?)),
            None => Ok(None),
        }
    }

    /// Record that `consumer` has been given everything up to `position`.
    pub fn set_topic_position(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        consumer: &str,
        position: u64,
    ) {
        let value = Value::Object(BTreeMap::from([
            (FIELD_TOPIC.to_owned(), Value::from(i64::from(table.get()))),
            (FIELD_CONSUMER.to_owned(), Value::from(consumer)),
            (
                FIELD_POSITION.to_owned(),
                Value::from(i64::try_from(position).unwrap_or(i64::MAX)),
            ),
        ]));
        self.transaction.put(
            system::address(
                system::TOPIC_POSITIONS,
                row(namespace, database, table, consumer),
            ),
            tessari_encoding::encode_payload(&value).into_bytes(),
        );
    }

    /// Every reader of this topic and the last position each was given, by
    /// name.
    ///
    /// # Errors
    ///
    /// A backend failure, or a stored row that is not a position.
    pub fn topic_positions(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<(String, u64)>> {
        let prefix = topic_prefix(namespace, database, table);
        let mut found = Vec::new();
        for (id, payload) in self.transaction.scan_table(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::TOPIC_POSITIONS,
        )? {
            let RecordId::Text(name) = &id else {
                continue;
            };
            if let Some(consumer) = name.strip_prefix(&prefix) {
                found.push((
                    consumer.to_owned(),
                    position_in(&decode_payload(&payload)?)?,
                ));
            }
        }
        Ok(found)
    }
}
