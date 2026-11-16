//! Where each declared outbound replication stands.
//!
//! A replication is a catalog entry like any other — an ordinary record in the
//! system tenancy (ADR-0009) — so declaring one takes part in the transaction
//! that issued it and reaches every node of this cluster through the same apply
//! path. What it never does is reach another cluster: the row names an external
//! party, and `carried_to` classifies it `StoreOnly`, which travels to every
//! subscriber of the store log and stops there. A cross-cluster link whose
//! definition rode the data channel would be a row the remote could not trust
//! the provenance of — so the link is declared on each side, and what crosses
//! the seam is only the database's log.
//!
//! # The name is the identity
//!
//! Rows are keyed by the name an operator wrote, not by an allocated number —
//! the rule ADR-0077 settled for peer rows, for the same reason: a number a
//! writing node hands out is one two writers can disagree about, and the table
//! is cluster-wide by its own header.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{Number, RecordId, Value};

use super::authority::{Reach, ReachCodec};
use super::definition::{field_name, object};
use super::{Catalog, JoinTicket, system};
use crate::error::{Error, Result};

const FIELD_NAME: &str = "name";
const FIELD_ENDPOINT: &str = "endpoint";
const FIELD_DATABASE: &str = "database";
const FIELD_REMOTE: &str = "remote";
const FIELD_INBOUND: &str = "inbound";
const FIELD_PAUSED: &str = "paused";
const FIELD_JOIN: &str = "join";
const FIELD_DIGEST: &str = "digest";
const FIELD_EXPIRES: &str = "expires";

const ENTITY: &str = "replication";

/// One database, outbound to another cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationDefinition {
    /// The name it is known by, unique across the `REPLICATIONS` table, and
    /// **the identity of the stored record** — the row lives at the key its own
    /// name gives it, for ADR-0077's reason.
    pub name: String,
    /// Where the remote cluster answers, as written.
    ///
    /// Stored as written for the reason a peer's endpoint is: whether a host
    /// and port resolves is a question for whoever dials it, and refusing an
    /// unreachable address at declaration time would make the statement's
    /// success depend on the network being up at the moment it ran.
    pub endpoint: String,
    /// The database this cluster ships to the remote one.
    ///
    /// Always a database reach: the unit of cross-cluster replication is one
    /// named database, because a database is the smallest scope that is
    /// self-naming — its records, topics and feeds resolve inside it, and
    /// the carried machinery already classifies a whole database's log.
    pub database: Reach,
    /// The database's name on the remote cluster (`AS 'shop'`), when said.
    /// `None` reads as the same name it has here.
    pub remote: Option<String>,
    /// The direction. `false` (`TO`, the default this field's absence reads
    /// as) is the outbound half — this cluster ships the database. `true`
    /// (`FROM`) is the inbound half: another cluster ships it, and this
    /// cluster's copy is read-only by declaration until the link is dropped.
    /// Written only when inbound, so a row declared before directions existed
    /// keeps its bytes.
    pub inbound: bool,
    /// Whether the stream is held (`ALTER REPLICATION … PAUSED`).
    ///
    /// Written only when true, so a row declared before pausing existed keeps
    /// its bytes. Inert until a driver consumes it — pausing is an
    /// operability surface, and the surface lands before the machinery does.
    pub paused: bool,
    /// A link token waiting to bind this row to the remote cluster's side
    /// (`CREATE LINK TOKEN`), as its digest and its expiry; cleared by the
    /// binding that spends it.
    pub join: Option<JoinTicket>,
}

impl ReplicationDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (
                FIELD_ENDPOINT.to_owned(),
                Value::from(self.endpoint.as_str()),
            ),
            (FIELD_DATABASE.to_owned(), self.database.to_value()),
        ]);
        if let Some(remote) = &self.remote {
            fields.insert(FIELD_REMOTE.to_owned(), Value::from(remote.as_str()));
        }
        if self.inbound {
            fields.insert(FIELD_INBOUND.to_owned(), Value::Bool(true));
        }
        if self.paused {
            fields.insert(FIELD_PAUSED.to_owned(), Value::Bool(true));
        }
        if let Some(join) = &self.join {
            fields.insert(
                FIELD_JOIN.to_owned(),
                Value::Object(BTreeMap::from([
                    (FIELD_DIGEST.to_owned(), Value::from(join.digest.as_str())),
                    (
                        FIELD_EXPIRES.to_owned(),
                        Value::Number(Number::Integer(join.expires_ms)),
                    ),
                ])),
            );
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let Some(database) = fields.get(FIELD_DATABASE) else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_DATABASE,
                found: "none",
            });
        };
        Ok(Self {
            name: field_name(fields, ENTITY)?,
            endpoint: text_of(fields, FIELD_ENDPOINT)?,
            database: Reach::from_value(database, ENTITY, FIELD_DATABASE)?,
            remote: text_in(fields, FIELD_REMOTE)?,
            inbound: flag_in(fields, FIELD_INBOUND)?,
            paused: flag_in(fields, FIELD_PAUSED)?,
            join: join_in(fields)?,
        })
    }
}

/// A required text field of a stored row: absent is malformed, because the
/// field has been part of the row since the table existed.
fn text_of(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<String> {
    match fields.get(field) {
        Some(Value::String(text)) => Ok(text.clone()),
        found => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: found.map_or("none", Value::type_name),
        }),
    }
}

/// An optional text field of a stored row: absent is `None`, and anything
/// present that is not text is refused rather than read as absent.
fn text_in(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<Option<String>> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: other.type_name(),
        }),
    }
}

/// A flag written only when true: absent is false, and anything present that
/// is not true or false is refused rather than read as either.
fn flag_in(fields: &BTreeMap<String, Value>, field: &'static str) -> Result<bool> {
    match fields.get(field) {
        None => Ok(false),
        Some(Value::Bool(set)) => Ok(*set),
        Some(other) => Err(Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: other.type_name(),
        }),
    }
}

/// The link token a stored row waits on: absent is none, anything else that
/// is not a digest and an expiry is refused rather than read as none.
fn join_in(fields: &BTreeMap<String, Value>) -> Result<Option<JoinTicket>> {
    let Some(found) = fields.get(FIELD_JOIN) else {
        return Ok(None);
    };
    let malformed = |field: &'static str, found: &Value| Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: found.type_name(),
    };
    let Value::Object(join) = found else {
        return Err(malformed(FIELD_JOIN, found));
    };
    match (join.get(FIELD_DIGEST), join.get(FIELD_EXPIRES)) {
        (Some(Value::String(digest)), Some(Value::Number(Number::Integer(expires_ms)))) => {
            Ok(Some(JoinTicket {
                digest: digest.clone(),
                expires_ms: *expires_ms,
            }))
        }
        _ => Err(malformed(FIELD_JOIN, found)),
    }
}

impl Catalog<'_, '_> {
    /// Declare an outbound replication from a whole definition.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NameTaken`] when the name is already declared.
    pub async fn create_replication_with(
        &mut self,
        definition: ReplicationDefinition,
    ) -> Result<ReplicationDefinition> {
        if self.replication_row(&definition.name).await?.is_some() {
            return Err(Error::NameTaken {
                holder: "a replication".to_owned(),
                name: definition.name.clone(),
            });
        }
        self.write_replication(&definition);
        Ok(definition)
    }

    /// The stored row a name is written under, read by that key.
    async fn replication_row(&self, name: &str) -> Result<Option<ReplicationDefinition>> {
        let Some(bytes) = self
            .transaction
            .get(&system::address(system::REPLICATIONS, RecordId::from(name)))
            .await?
        else {
            return Ok(None);
        };
        ReplicationDefinition::from_value(&decode_payload(&bytes)?).map(Some)
    }

    /// Write a row at the key its own name gives it (ADR-0077).
    fn write_replication(&mut self, definition: &ReplicationDefinition) {
        self.transaction.put(
            system::address(
                system::REPLICATIONS,
                RecordId::from(definition.name.as_str()),
            ),
            encode_payload(&definition.to_value()).into_bytes(),
        );
    }

    /// Wait on a link token for `name`'s row (`CREATE LINK TOKEN`).
    ///
    /// Answers `false` when there is no row under that name.
    pub async fn wait_for_link(&mut self, name: &str, ticket: JoinTicket) -> Result<bool> {
        let Some(mut definition) = self.replication_row(name).await? else {
            return Ok(false);
        };
        definition.join = Some(ticket);
        self.write_replication(&definition);
        Ok(true)
    }

    /// Remove a replication's declaration and release its name.
    ///
    /// Answers `false` when there was nothing under that name. Removing the
    /// declaration is all this does — the remote cluster is not told and its
    /// copy is not chased, for the reason `DROP REPLICA` gives: this statement
    /// says *we no longer count that link*, and a remote that disagrees is an
    /// operator's question, not one a catalog write can settle.
    pub async fn drop_replication(&mut self, name: &str) -> Result<bool> {
        if self.replication_row(name).await?.is_none() {
            return Ok(false);
        }
        self.transaction
            .delete(system::address(system::REPLICATIONS, RecordId::from(name)));
        Ok(true)
    }

    /// Amend the row named `name` in place with `amend`, and answer whether
    /// there was one.
    pub async fn amend_replication(
        &mut self,
        name: &str,
        amend: impl FnOnce(&mut ReplicationDefinition),
    ) -> Result<bool> {
        let Some(mut definition) = self.replication_row(name).await? else {
            return Ok(false);
        };
        amend(&mut definition);
        self.write_replication(&definition);
        Ok(true)
    }

    /// Every declared replication, in name order.
    ///
    /// Sorted for the reason `replicas` sorts: two nodes comparing what they
    /// think the cluster says is the point of the table, and an answer whose
    /// shape depends on declaration order is two answers to one question.
    ///
    /// # Errors
    ///
    /// Returns an error when a stored definition cannot be read.
    pub async fn replications(&self) -> Result<Vec<ReplicationDefinition>> {
        let mut found = Vec::new();
        for (_, payload) in self
            .transaction
            .scan_table(
                system::SYSTEM_NAMESPACE,
                system::SYSTEM_DATABASE,
                system::REPLICATIONS,
            )
            .await?
        {
            found.push(ReplicationDefinition::from_value(&decode_payload(
                &payload,
            )?)?);
        }
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(found)
    }
}
