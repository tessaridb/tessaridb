//! The realm/domain scheme a cluster is arranged over, and the pair that orders it.
//!
//! # Why a row rather than a set of labels
//!
//! A `REGION` on a member row is a free-text label: an acknowledge level counts
//! it, and nothing can validate it, because the cluster does not know which
//! rows form a realm or how many failure domains a realm spans. The declared
//! topology is that knowledge as catalog data, and it is a **cluster-wide
//! fact** for [`super::failover`]'s reason: the claim *losing this realm is
//! survived* is a claim about the membership every node holds, so two nodes
//! disagreeing about the scheme would disagree about what a layout promises —
//! a configuration file's exact failure, one layer up. It is a statement and a
//! replicated row.
//!
//! # What is checked where
//!
//! The row stores the structure checked by [`Topology::stated`] — a stored row
//! is bytes on a disk, and bytes can be older than the relations, or edited,
//! so [`TopologyDefinition::from_value`] re-states the value rather than
//! trusting it. The relations that need the membership — the claim arithmetic,
//! the copies-span-realms fit, the acknowledge fit — are checked where the
//! replicas and namespaces are readable: the session's write of the row, which
//! is the next layer up from this one.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{Epoch, Number, RecordId, Value};

use super::definition::{count_of, object};
use super::{Catalog, system};
use crate::error::{Error, Result};
use crate::topology::{Realm, Topology};

const FIELD_NAME: &str = "name";
const FIELD_REALMS: &str = "realms";
const FIELD_EPOCH: &str = "epoch";
const FIELD_VERSION: &str = "version";
/// Written only when set, so a scheme stored before the claim existed reads
/// back byte-identical.
const FIELD_SURVIVES: &str = "survives_realm_loss";

const ENTITY: &str = "topology";

/// The one row this table holds.
///
/// A fixed handle rather than a generated id, because there is exactly one
/// scheme per cluster: a second row would be a second answer to a question
/// that admits one, and nothing downstream would know which to read.
const ROW: &str = "scheme";

/// A declared topology as the log records it: the scheme, the leadership that
/// wrote it, and which setting under that leadership this was.
///
/// The pair orders the row for [`FailoverDefinition`]'s reason — neither number
/// is a clock, and the two cases the pair settles are the same two cases.
///
/// [`FailoverDefinition`]: super::FailoverDefinition
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyDefinition {
    /// The name the scheme is known by.
    pub name: String,
    /// The realms and domains, already checked as a structure.
    ///
    /// A [`Topology`] in hand is one whose structural relations hold, because
    /// [`Topology::stated`] is the only way to build one — so a row read back
    /// is re-checked at [`Self::from_value`] rather than trusted.
    pub topology: Topology,
    /// The leadership this scheme was written under.
    pub epoch: Epoch,
    /// Which setting under that leadership this was.
    pub version: u64,
}

impl TopologyDefinition {
    /// The record the scheme is stored under.
    #[must_use]
    pub fn key() -> RecordId {
        RecordId::from(ROW)
    }

    /// The value written to the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the epoch or the version is
    /// past `i64::MAX`, the widest integer a stored value holds. Refused
    /// rather than truncated, for [`super::failover`]'s reason: a value that
    /// wrapped compares as older than the one before it, and every ordering
    /// rule in the cluster reads that comparison.
    pub fn to_value(&self) -> Result<Value> {
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: "number",
        };
        let count =
            |field: &'static str, value: u64| i64::try_from(value).map_err(|_| malformed(field));
        let realms = Value::Array(
            self.topology
                .realms()
                .iter()
                .map(|realm| {
                    Value::Object(BTreeMap::from([
                        (
                            FIELD_NAME.to_owned(),
                            Value::String(realm.name().to_owned()),
                        ),
                        (
                            "domains".to_owned(),
                            Value::Array(
                                realm
                                    .domains()
                                    .iter()
                                    .map(|domain| Value::String(domain.clone()))
                                    .collect(),
                            ),
                        ),
                    ]))
                })
                .collect(),
        );
        let mut fields = BTreeMap::from([
            (FIELD_NAME.to_owned(), Value::String(self.name.clone())),
            (FIELD_REALMS.to_owned(), realms),
            (
                FIELD_EPOCH.to_owned(),
                Value::Number(Number::Integer(count(FIELD_EPOCH, self.epoch.get())?)),
            ),
            (
                FIELD_VERSION.to_owned(),
                Value::Number(Number::Integer(count(FIELD_VERSION, self.version)?)),
            ),
        ]);
        if self.topology.survives_realm_loss() {
            fields.insert(FIELD_SURVIVES.to_owned(), Value::Bool(true));
        }
        Ok(Value::Object(fields))
    }

    /// Read a definition back, re-checking the structure.
    ///
    /// Every realm goes back through [`Realm::stated`] and the whole through
    /// [`Topology::stated`], so a row whose structure no longer holds together
    /// is refused at the boundary instead of becoming a scheme nothing ever
    /// checked. That row can exist: it may have been written by a build whose
    /// relations differed, or edited.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds
    /// the wrong shape, and the structure's own refusal when the realms
    /// disagree.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |field: &'static str, found: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found,
        };
        let Some(Value::String(name)) = fields.get(FIELD_NAME) else {
            return Err(malformed(
                FIELD_NAME,
                fields.get(FIELD_NAME).map_or("none", Value::type_name),
            ));
        };
        let Some(Value::Array(stored)) = fields.get(FIELD_REALMS) else {
            return Err(malformed(
                FIELD_REALMS,
                fields.get(FIELD_REALMS).map_or("none", Value::type_name),
            ));
        };
        let mut realms = Vec::with_capacity(stored.len());
        for held in stored {
            let Value::Object(realm) = held else {
                return Err(malformed(FIELD_REALMS, held.type_name()));
            };
            let Some(Value::String(realm_name)) = realm.get(FIELD_NAME) else {
                return Err(malformed(
                    FIELD_NAME,
                    realm.get(FIELD_NAME).map_or("none", Value::type_name),
                ));
            };
            let domains = match realm.get("domains") {
                Some(Value::Array(domains)) => domains
                    .iter()
                    .map(|domain| match domain {
                        Value::String(domain) => Ok(domain.clone()),
                        other => Err(malformed("domains", other.type_name())),
                    })
                    .collect::<Result<Vec<_>>>()?,
                found => {
                    return Err(malformed("domains", found.map_or("none", Value::type_name)));
                }
            };
            realms.push(Realm::stated(realm_name.clone(), domains)?);
        }
        let Some(epoch) = fields.get(FIELD_EPOCH) else {
            return Err(malformed(FIELD_EPOCH, "none"));
        };
        let Some(version) = fields.get(FIELD_VERSION) else {
            return Err(malformed(FIELD_VERSION, "none"));
        };
        Ok(Self {
            name: name.clone(),
            topology: Topology::stated(
                realms,
                match fields.get(FIELD_SURVIVES) {
                    None => false,
                    Some(Value::Bool(survives)) => *survives,
                    Some(_) => return Err(malformed(FIELD_SURVIVES, "not a bool")),
                },
            )?,
            epoch: Epoch::new(count_of(epoch, ENTITY, FIELD_EPOCH)?),
            version: count_of(version, ENTITY, FIELD_VERSION)?,
        })
    }
}

impl Catalog<'_, '_> {
    /// Record the scheme this cluster is arranged over.
    ///
    /// The caller supplies the pair, for [`Catalog::set_failover`]'s reason: it
    /// is not derived here because the two fields answer to different owners,
    /// and a catalog method that invented either would be a second place the
    /// ordering is decided.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the definition cannot be
    /// stored; see [`TopologyDefinition::to_value`].
    pub fn set_topology(
        &mut self,
        name: String,
        topology: Topology,
        epoch: Epoch,
        version: u64,
    ) -> Result<TopologyDefinition> {
        let definition = TopologyDefinition {
            name,
            topology,
            epoch,
            version,
        };
        self.transaction.put(
            system::address(system::TOPOLOGY, TopologyDefinition::key()),
            encode_payload(&definition.to_value()?).into_bytes(),
        );
        Ok(definition)
    }

    /// The scheme the log says this cluster is arranged over, or `None` when
    /// nobody has declared one.
    ///
    /// `None` is a real answer: member-row labels are counted without a scheme
    /// today, and reporting an undeclared scheme as *the empty scheme* would
    /// make it impossible to see whether one ever arrived.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub async fn topology(&self) -> Result<Option<TopologyDefinition>> {
        let Some(payload) = self
            .transaction
            .get(&system::address(
                system::TOPOLOGY,
                TopologyDefinition::key(),
            ))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(TopologyDefinition::from_value(&decode_payload(
            &payload,
        )?)?))
    }

    /// Remove the declared scheme. Answers `false` when there was none.
    ///
    /// Removing the row never touches member rows: their `REGION` and `DOMAIN`
    /// labels read as what they are — labels — until a new scheme declares
    /// them.
    pub async fn drop_topology(&mut self) -> Result<bool> {
        let key = system::address(system::TOPOLOGY, TopologyDefinition::key());
        if self.transaction.get(&key).await?.is_none() {
            return Ok(false);
        }
        self.transaction.delete(key);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use tessari_types::Epoch;

    use super::*;

    fn scheme() -> TopologyDefinition {
        let topology = Topology::stated(
            vec![
                Realm::stated(
                    "dc-a".to_owned(),
                    vec!["az-2".to_owned(), "az-1".to_owned()],
                )
                .expect("a well-formed realm"),
                Realm::stated("dc-b".to_owned(), vec![]).expect("a well-formed realm"),
            ],
            true,
        )
        .expect("a topology inside every structural relation");
        TopologyDefinition {
            name: "main".to_owned(),
            topology,
            epoch: Epoch::new(7),
            version: 3,
        }
    }

    #[test]
    fn a_definition_round_trips_through_its_stored_value() {
        let definition = scheme();
        let read = TopologyDefinition::from_value(&definition.to_value().expect("encodable"))
            .expect("a stored row reads back");

        assert_eq!(read, definition);
        // The domains read back in the checked set's order — the set's order,
        // not the statement's, because a set has no second order to drift.
        assert_eq!(
            read.topology.realm("dc-a").expect("held").domains(),
            ["az-1", "az-2"]
        );
        assert!(read.topology.survives_realm_loss());
    }

    #[test]
    fn a_stored_structure_that_no_longer_holds_is_refused_at_the_boundary() {
        // A row edited on disk — two realms with one name — is refused by the
        // read-back's re-check, not served as a scheme nothing checked.
        let mut value = match scheme().to_value().expect("encodable") {
            Value::Object(fields) => fields,
            other => panic!("a definition encodes as an object, got {other:?}"),
        };
        let Some(Value::Array(realms)) = value.get_mut(FIELD_REALMS) else {
            panic!("the realms encode as an array");
        };
        let second = realms[1].clone();
        realms.push(second);
        let refused = TopologyDefinition::from_value(&Value::Object(value))
            .expect_err("a duplicated realm read back");

        assert!(
            matches!(refused, Error::TopologyRealmNamedTwice { .. }),
            "expected the structural refusal, got {refused}"
        );
    }
}
