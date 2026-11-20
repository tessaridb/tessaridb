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
use tessari_types::{Number, RecordId, Sequence, Value};

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
const FIELD_POSITION: &str = "position";
const FIELD_STATE: &str = "state";
const FIELD_SEED_AT: &str = "seed_at";
const FIELD_DIGEST: &str = "digest";
const FIELD_EXPIRES: &str = "expires";

const ENTITY: &str = "replication";

/// Where a seeded link stands in its catch-up lifecycle.
///
/// The lifecycle opens with `DEFINE REPLICATION … SEED AT <position>`: the
/// position names where the receiving copy's snapshot ends, and the link is
/// `Seeding` until the copy holds everything through it. `Standby` is the
/// caught-up state — the copy serves reads — and `Active` is the ordinary
/// running state, reached with `ALTER REPLICATION … ACTIVE` once the source's
/// tail no longer runs ahead of what the far side has confirmed. A link
/// declared without a seed position is `Active` from birth: the state field
/// is written only when it says something other than that default, so a row
/// declared before the lifecycle existed keeps its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationState {
    /// The copy has not yet caught up to the seed position.
    Seeding,
    /// The copy holds everything through the seed position; reads serve.
    Standby,
    /// The ordinary running state — today's RESUMED behavior, named.
    Active,
}

impl ReplicationState {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seeding => "seeding",
            Self::Standby => "standby",
            Self::Active => "active",
        }
    }

    /// Read a stored spelling; anything else is a row that was edited, refused
    /// rather than read as a default, for [`ReplicationDefinition`]'s reason.
    fn from_str(found: &str) -> Result<Self> {
        match found {
            "seeding" => Ok(Self::Seeding),
            "standby" => Ok(Self::Standby),
            "active" => Ok(Self::Active),
            _ => Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_STATE,
                found: "text",
            }),
        }
    }
}

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
    /// The confirmed position on an OUTBOUND row: the sequence the remote
    /// cluster has durably applied, as far as this cluster has been told.
    /// `None` is the row the driver has not shipped from yet — the link
    /// starts at the log's origin, the way a new follower does, and the whole
    /// history crosses the seam once. Written only when the driver has
    /// something to say, so a row declared before positions existed keeps its
    /// bytes. At-least-once means a restart re-ships from here rather than
    /// from the origin, and the apply is idempotent by identity.
    pub position: Option<Sequence>,
    /// Where the link stands in its seeding lifecycle. `Active` — the
    /// ordinary running state — is the default a row without the field
    /// reads as, so a row declared before the lifecycle existed keeps its
    /// bytes and its meaning.
    pub state: ReplicationState,
    /// The position the receiving copy's snapshot ends at (`DEFINE
    /// REPLICATION … SEED AT <position>`), when the statement named one. On
    /// an OUTBOUND row it is the floor the source serves the tail from; on
    /// an INBOUND row it is the floor the copy's catch-up is measured
    /// against. `None` is the link without a seeding story, which is active
    /// from birth.
    pub seed_at: Option<Sequence>,
}

impl ReplicationDefinition {
    /// The value written to the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the confirmed position is past
    /// `i64::MAX`, the widest integer a stored value holds. Refused rather than
    /// truncated, for [`super::topology`]'s reason: a wrapped cursor reads as
    /// behind the origin, and a resumed link would re-ship history the remote
    /// holds.
    pub fn to_value(&self) -> Result<Value> {
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
        if let Some(position) = self.position {
            let held = i64::try_from(position.get()).map_err(|_| Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_POSITION,
                found: "number",
            })?;
            fields.insert(
                FIELD_POSITION.to_owned(),
                Value::Number(Number::Integer(held)),
            );
        }
        if self.state != ReplicationState::Active {
            fields.insert(FIELD_STATE.to_owned(), Value::from(self.state.as_str()));
        }
        if let Some(seed_at) = self.seed_at {
            let held = i64::try_from(seed_at.get()).map_err(|_| Error::CatalogMalformed {
                entity: ENTITY,
                field: FIELD_SEED_AT,
                found: "number",
            })?;
            fields.insert(
                FIELD_SEED_AT.to_owned(),
                Value::Number(Number::Integer(held)),
            );
        }
        Ok(Value::Object(fields))
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
            position: match fields.get(FIELD_POSITION) {
                None => None,
                Some(Value::Number(Number::Integer(held))) => {
                    Some(Sequence::new(u64::try_from(*held).map_err(|_| {
                        Error::CatalogMalformed {
                            entity: ENTITY,
                            field: FIELD_POSITION,
                            found: "number",
                        }
                    })?))
                }
                Some(_) => {
                    return Err(Error::CatalogMalformed {
                        entity: ENTITY,
                        field: FIELD_POSITION,
                        found: "not a number",
                    });
                }
            },
            state: match fields.get(FIELD_STATE) {
                None => ReplicationState::Active,
                Some(Value::String(held)) => ReplicationState::from_str(held)?,
                Some(_) => {
                    return Err(Error::CatalogMalformed {
                        entity: ENTITY,
                        field: FIELD_STATE,
                        found: "not text",
                    });
                }
            },
            seed_at: match fields.get(FIELD_SEED_AT) {
                None => None,
                Some(Value::Number(Number::Integer(held))) => {
                    Some(Sequence::new(u64::try_from(*held).map_err(|_| {
                        Error::CatalogMalformed {
                            entity: ENTITY,
                            field: FIELD_SEED_AT,
                            found: "number",
                        }
                    })?))
                }
                Some(_) => {
                    return Err(Error::CatalogMalformed {
                        entity: ENTITY,
                        field: FIELD_SEED_AT,
                        found: "not a number",
                    });
                }
            },
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
        self.write_replication(&definition)?;
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
    /// Persist `definition`, refused rather than written when its confirmed
    /// position cannot be stored (see [`ReplicationDefinition::to_value`]).
    fn write_replication(&mut self, definition: &ReplicationDefinition) -> Result<()> {
        self.transaction.put(
            system::address(
                system::REPLICATIONS,
                RecordId::from(definition.name.as_str()),
            ),
            encode_payload(&definition.to_value()?).into_bytes(),
        );
        Ok(())
    }

    /// Wait on a link token for `name`'s row (`CREATE LINK TOKEN`).
    ///
    /// Answers `false` when there is no row under that name.
    pub async fn wait_for_link(&mut self, name: &str, ticket: JoinTicket) -> Result<bool> {
        let Some(mut definition) = self.replication_row(name).await? else {
            return Ok(false);
        };
        definition.join = Some(ticket);
        self.write_replication(&definition)?;
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
        self.write_replication(&definition)?;
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

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_types::{NamespaceId, Sequence};

    use super::*;

    fn outbound() -> ReplicationDefinition {
        ReplicationDefinition {
            name: "prod_dr".to_owned(),
            endpoint: "dr:9080".to_owned(),
            database: Reach::Database(NamespaceId::new(1), tessari_types::DatabaseId::new(2)),
            remote: None,
            inbound: false,
            paused: false,
            join: None,
            position: None,
            state: ReplicationState::Active,
            seed_at: None,
        }
    }

    #[test]
    fn a_definition_without_a_position_reads_back_byte_identical() {
        // The field lands after the declaration did: a row written before it
        // exists keeps its bytes, and the absent cursor reads as the driver
        // starting the link at the log's origin.
        let definition = outbound();
        let read = ReplicationDefinition::from_value(&definition.to_value().expect("encodable"))
            .expect("a row without a position reads back");

        assert_eq!(read, definition);
        assert_eq!(read.position, None);
    }

    #[test]
    fn a_seeded_row_round_trips_its_lifecycle_through_the_stored_value() {
        // The state machine rides the row: the seed position and a state
        // other than the default survive the stored value, and the active
        // default stays absent from the bytes entirely.
        let mut definition = outbound();
        definition.seed_at = Some(Sequence::new(8128));
        definition.state = ReplicationState::Seeding;
        let read = ReplicationDefinition::from_value(&definition.to_value().expect("encodable"))
            .expect("a seeded row reads back");

        assert_eq!(read, definition);
        assert_eq!(read.state, ReplicationState::Seeding);
        assert_eq!(read.seed_at, Some(Sequence::new(8128)));
    }

    #[test]
    fn a_seeded_row_carries_no_state_field_for_the_active_default() {
        // Written only when it says something: an active row — the default a
        // row without the field reads as — keeps the bytes a declaration
        // written before the lifecycle existed would have written.
        let mut definition = outbound();
        definition.seed_at = Some(Sequence::new(64));
        let encoded = definition.to_value().expect("encodable");
        let Value::Object(fields) = &encoded else {
            panic!("a definition encodes as an object, got {encoded:?}");
        };
        assert!(
            !fields.contains_key(FIELD_STATE),
            "the active default is written only when it says something: {fields:?}"
        );
        let read = ReplicationDefinition::from_value(&encoded).expect("a row reads back");
        assert_eq!(read.state, ReplicationState::Active);
    }

    #[test]
    fn a_state_that_is_not_a_lifecycle_is_refused_not_read_as_active() {
        // Anything present that is not one of the three spellings is a row
        // that was edited, and reading it as *active* would promote a link
        // the operator never promoted.
        let mut value = match outbound().to_value().expect("encodable") {
            Value::Object(fields) => fields,
            other => panic!("a definition encodes as an object, got {other:?}"),
        };
        value.insert(FIELD_STATE.to_owned(), Value::from("promoted"));
        let refused = ReplicationDefinition::from_value(&Value::Object(value))
            .expect_err("an unknown state read back");

        assert!(
            matches!(refused, Error::CatalogMalformed { .. }),
            "expected the malformed refusal, got {refused}"
        );
    }

    #[test]
    fn a_seed_position_that_is_not_a_number_is_refused_not_read_as_absent() {
        // Anything present that is not a number is a row that was edited, and
        // reading it as *no seed* would serve the whole history the copy
        // holds.
        let mut value = match outbound().to_value().expect("encodable") {
            Value::Object(fields) => fields,
            other => panic!("a definition encodes as an object, got {other:?}"),
        };
        value.insert(FIELD_SEED_AT.to_owned(), Value::from("8128"));
        let refused = ReplicationDefinition::from_value(&Value::Object(value))
            .expect_err("a text seed position read back");

        assert!(
            matches!(refused, Error::CatalogMalformed { .. }),
            "expected the malformed refusal, got {refused}"
        );
    }

    #[test]
    fn a_confirmed_position_round_trips_through_the_stored_value() {
        // The shipper's resume cursor: written when the remote confirms an
        // apply, read back at restart so the link continues where it was
        // acknowledged rather than re-shipping from the origin.
        let mut definition = outbound();
        definition.position = Some(Sequence::new(8128));
        let read = ReplicationDefinition::from_value(&definition.to_value().expect("encodable"))
            .expect("a row with a position reads back");

        assert_eq!(read, definition);
        assert_eq!(read.position, Some(Sequence::new(8128)));
    }

    #[test]
    fn a_position_that_is_not_a_number_is_refused_not_read_as_absent() {
        // Anything present that is not a number is a row that was edited, and
        // reading it as *no cursor* would re-ship a history the remote holds.
        let mut value = match outbound().to_value().expect("encodable") {
            Value::Object(fields) => fields,
            other => panic!("a definition encodes as an object, got {other:?}"),
        };
        value.insert(FIELD_POSITION.to_owned(), Value::from("8128"));
        let refused = ReplicationDefinition::from_value(&Value::Object(value))
            .expect_err("a text position read back");

        assert!(
            matches!(refused, Error::CatalogMalformed { .. }),
            "expected the malformed refusal, got {refused}"
        );
    }
}
