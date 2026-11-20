//! The workload pools as the log records them: one row per (namespace,
//! name), beside [`super::balancer`]'s row for the same reason — whose
//! statement may run is a cluster-wide fact every node must agree about.
//!
//! # The key
//!
//! A replica row keys by its name because a peer's name is store-wide
//! unique. A pool's name is unique only within its namespace, so the key
//! carries both: `namespace id : name`. Two namespaces may each hold a pool
//! called `web` without the rows ever meeting.

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{DatabaseId, NamespaceId, Number, PoolKinds, RecordId, Value};

use super::definition::{count_of, object};
use super::{Catalog, system};
use crate::error::{Error, Result};
use crate::pool::Pool;

const FIELD_NAME: &str = "name";
const FIELD_LIMIT: &str = "max_concurrent";
const FIELD_NAMESPACE: &str = "namespace";
const FIELD_DATABASE: &str = "database";
const FIELD_USER: &str = "user";
const FIELD_READS: &str = "reads";
const FIELD_WRITES: &str = "writes";
const FIELD_DEFINITIONS: &str = "definitions";

const ENTITY: &str = "pool";

/// A pool as the catalog holds it: the checked value with the namespace and
/// name the key carries restated in the row, the way analyzer rows restate
/// theirs, so a listing reads the value alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDefinition {
    /// The namespace the pool belongs to.
    pub namespace: NamespaceId,
    /// The name, unique within the namespace.
    pub name: String,
    /// The limit and classifier, already checked against one another.
    pub pool: Pool,
}

impl PoolDefinition {
    /// The record the definition is stored under: the namespace id and the
    /// name, one key, for the module's reason.
    #[must_use]
    pub fn key(namespace: NamespaceId, name: &str) -> RecordId {
        RecordId::Text(format!("{}:{name}", namespace.get()))
    }

    /// The definition as a catalog record's value.
    ///
    /// # Errors
    ///
    /// Returns an error when a field does not fit the store's spelling —
    /// which [`Pool::stated`] already held, so this is the re-read's
    /// defence, not the write's.
    pub fn to_value(&self) -> Result<Value> {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(FIELD_NAME.to_owned(), Value::from(self.name.as_str()));
        fields.insert(
            FIELD_NAMESPACE.to_owned(),
            Value::Number(Number::Integer(i64::from(self.namespace.get()))),
        );
        fields.insert(
            FIELD_LIMIT.to_owned(),
            Value::Number(Number::Integer(
                i64::try_from(self.pool.max_concurrent()).map_err(|_| Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_LIMIT,
                    found: "a limit past any count",
                })?,
            )),
        );
        if let Some(database) = self.pool.database() {
            fields.insert(
                FIELD_DATABASE.to_owned(),
                Value::Number(Number::Integer(i64::from(database.get()))),
            );
        }
        if let Some(user) = self.pool.user() {
            fields.insert(FIELD_USER.to_owned(), Value::from(user));
        }
        let kinds = self.pool.kinds();
        fields.insert(FIELD_READS.to_owned(), Value::Bool(kinds.reads));
        fields.insert(FIELD_WRITES.to_owned(), Value::Bool(kinds.writes));
        fields.insert(FIELD_DEFINITIONS.to_owned(), Value::Bool(kinds.definitions));
        Ok(Value::Object(fields))
    }

    /// The definition a catalog record's value holds, re-checked rather than
    /// trusted: a row on disk can be older than the relations, or edited.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is absent or malformed, and when the
    /// limit no longer holds.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: fields.get(field).map_or("none", Value::type_name),
        };
        let text_of = |field: &'static str| match fields.get(field) {
            Some(Value::String(held)) => Ok(held.clone()),
            _ => Err(malformed(field)),
        };
        let Some(limit) = fields.get(FIELD_LIMIT) else {
            return Err(malformed(FIELD_LIMIT));
        };
        let Some(namespace) = fields.get(FIELD_NAMESPACE) else {
            return Err(malformed(FIELD_NAMESPACE));
        };
        let flag = |field: &'static str| match fields.get(field) {
            Some(Value::Bool(held)) => Ok(*held),
            _ => Err(malformed(field)),
        };
        Ok(Self {
            namespace: NamespaceId::new(
                u32::try_from(count_of(namespace, ENTITY, FIELD_NAMESPACE)?)
                    .map_err(|_| malformed(FIELD_NAMESPACE))?,
            ),
            name: text_of(FIELD_NAME)?,
            pool: Pool::stated(
                count_of(limit, ENTITY, FIELD_LIMIT)?,
                match fields.get(FIELD_DATABASE) {
                    None => None,
                    Some(Value::Number(Number::Integer(held))) => Some(DatabaseId::new(
                        u32::try_from(*held).map_err(|_| malformed(FIELD_DATABASE))?,
                    )),
                    Some(_) => return Err(malformed(FIELD_DATABASE)),
                },
                match fields.get(FIELD_USER) {
                    None => None,
                    Some(Value::String(held)) => Some(held.clone()),
                    Some(_) => return Err(malformed(FIELD_USER)),
                },
                PoolKinds {
                    reads: flag(FIELD_READS)?,
                    writes: flag(FIELD_WRITES)?,
                    definitions: flag(FIELD_DEFINITIONS)?,
                },
            )?,
        })
    }
}

impl Catalog<'_, '_> {
    /// Write a pool under its (namespace, name) key, refused when the name is
    /// taken in that namespace — the replica row's `NameTaken` shape.
    pub async fn create_pool(
        &mut self,
        namespace: NamespaceId,
        name: &str,
        pool: Pool,
    ) -> Result<PoolDefinition> {
        let key = system::address(system::POOLS, PoolDefinition::key(namespace, name));
        if self.transaction.get(&key).await?.is_some() {
            return Err(Error::NameTaken {
                holder: "a pool".to_owned(),
                name: name.to_owned(),
            });
        }
        let definition = PoolDefinition {
            namespace,
            name: name.to_owned(),
            pool,
        };
        self.transaction
            .put(key, encode_payload(&definition.to_value()?).into_bytes());
        Ok(definition)
    }

    /// Remove a pool, answering whether one was held — the shape
    /// [`super::topology`]'s drop answers.
    pub async fn drop_pool(&mut self, namespace: NamespaceId, name: &str) -> Result<bool> {
        let key = system::address(system::POOLS, PoolDefinition::key(namespace, name));
        if self.transaction.get(&key).await?.is_none() {
            return Ok(false);
        }
        self.transaction.delete(key);
        Ok(true)
    }

    /// Every pool a namespace holds, newest key first.
    pub async fn pools(&self, namespace: NamespaceId) -> Result<Vec<PoolDefinition>> {
        let mut held = Vec::new();
        for (_, payload) in self
            .transaction
            .scan_table(
                system::SYSTEM_NAMESPACE,
                system::SYSTEM_DATABASE,
                system::POOLS,
            )
            .await?
        {
            let definition = PoolDefinition::from_value(&decode_payload(&payload)?)?;
            if definition.namespace == namespace {
                held.push(definition);
            }
        }
        Ok(held)
    }
}
