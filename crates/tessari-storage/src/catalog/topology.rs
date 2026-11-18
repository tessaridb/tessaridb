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
//! the copies-span-realms fit, the acknowledge fit — are [`check_topology`], a
//! pure check over views the caller assembles from the catalog rows it can
//! read: they run at the session's write of the scheme and of every member
//! row, which is where the replicas and namespaces are readable.

use std::collections::BTreeMap;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{Acknowledge, Epoch, NamespaceId, Number, RecordId, Value};

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

/// A member as the relations read it: its labels, whether it votes, and which
/// namespaces it holds copies of.
///
/// Assembled by the caller from the catalog rows it can read — one view per
/// member row, plus one for the node the check runs on, which holds its own
/// namespaces whether or not a row says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    /// The row's name — `self` for the node the check runs on.
    pub row: String,
    /// The region label (`REGION`), when the row names one.
    pub region: Option<String>,
    /// The domain label (`DOMAIN`), when the row names one.
    pub domain: Option<String>,
    /// Whether the member votes — the `coordinating` role.
    pub coordinating: bool,
    /// Which namespaces the member holds copies of.
    pub holds: MemberHolds,
}

/// Which namespaces a member holds copies of, from its `REPLICATES` reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberHolds {
    /// `REPLICATES STORE` — every namespace.
    Store,
    /// `REPLICATES NAMESPACE …` / `DATABASE` / `SHARD` — the listed ones.
    Namespaces(Vec<NamespaceId>),
    /// No subscription — the member holds no copies.
    Nothing,
}

/// A namespace as the relations read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceView {
    /// The namespace's id — what holders are matched on.
    pub id: NamespaceId,
    /// Its name, for refusals.
    pub name: String,
    /// The copies the cluster is asked to keep. `1` reads as a single copy,
    /// which spans nothing and needs to; a namespace that never stated its
    /// replication is not checked, for the worksheet's third-state rule.
    pub copies: u32,
    /// The acknowledged level, when stated.
    pub acknowledge: Option<Acknowledge>,
}

/// The checked relations R1–R4, as a set, against the membership as it stands.
///
/// Pure so the matrix is exact: the caller assembles the views from the
/// catalog rows it can read and the relations answer without I/O. Every
/// refusal names the relation it broke and the direction — the row and label,
/// the realm and its arithmetic, the namespace and its single realm, the
/// namespace and its region's voters — because a check a caller cannot act on
/// is a complaint.
///
/// R1 (no orphan labels) always runs: a scheme declares the labels, so the
/// labels must be the scheme's. R2 (the claim arithmetic) runs only when the
/// scheme claims `SURVIVES REALM LOSS` — silence is a third state, and an
/// unclaimed scheme is never judged. R3 (copies span realms) runs for every
/// namespace asked to keep more than one copy. R4 (acknowledge fit) runs for
/// every namespace acknowledging `LOCAL MAJORITY`, against the leader's
/// region; a leader naming no region is the write guard's own refusal, not
/// this check's, so the check stays silent there.
///
/// # Errors
///
/// [`Error::TopologyOrphanLabel`], [`Error::TopologyClaimBreaks`],
/// [`Error::TopologyCopiesInOneRealm`] or [`Error::TopologyAcknowledgeWithoutVoters`],
/// whichever relation fails first, in that order.
pub fn check_topology(
    scheme: &Topology,
    members: &[MemberView],
    namespaces: &[NamespaceView],
    leader_region: Option<&str>,
) -> Result<()> {
    for member in members {
        if let Some(region) = &member.region {
            let Some(realm) = scheme.realm(region) else {
                return Err(Error::TopologyOrphanLabel {
                    row: member.row.clone(),
                    label: region.clone(),
                    what: "region",
                });
            };
            if let Some(domain) = &member.domain
                && !realm.has_domain(domain)
            {
                return Err(Error::TopologyOrphanLabel {
                    row: member.row.clone(),
                    label: domain.clone(),
                    what: "domain",
                });
            }
        }
    }

    if scheme.survives_realm_loss() {
        let voters: Vec<&MemberView> = members
            .iter()
            .filter(|member| member.coordinating)
            .collect();
        let total = voters.len();
        for realm in scheme.realms() {
            // A voter no realm claims is outside every realm: it survives any
            // single loss, which is what the arithmetic counts.
            let outside = voters
                .iter()
                .filter(|voter| voter.region.as_deref() != Some(realm.name()))
                .count();
            if outside.saturating_mul(2) <= total {
                return Err(Error::TopologyClaimBreaks {
                    realm: realm.name().to_owned(),
                    left: outside,
                    total,
                });
            }
        }
    }

    for namespace in namespaces {
        if namespace.copies < 2 {
            continue;
        }
        let mut realms: Vec<&str> = Vec::new();
        for member in members {
            let holds = match &member.holds {
                MemberHolds::Store => true,
                MemberHolds::Namespaces(held) => held.contains(&namespace.id),
                MemberHolds::Nothing => false,
            };
            if holds
                && let Some(region) = &member.region
                && !realms.contains(&region.as_str())
            {
                realms.push(region);
            }
        }
        if realms.len() <= 1 {
            return Err(Error::TopologyCopiesInOneRealm {
                namespace: namespace.name.clone(),
                realm: realms.first().map(|realm| (*realm).to_owned()),
            });
        }
    }

    if let Some(region) = leader_region {
        for namespace in namespaces {
            let Some(Acknowledge::LocalMajority) = namespace.acknowledge else {
                continue;
            };
            let in_region: Vec<&MemberView> = members
                .iter()
                .filter(|member| member.region.as_deref() == Some(region))
                .collect();
            let voters = in_region
                .iter()
                .filter(|member| member.coordinating)
                .count();
            if voters.saturating_mul(2) <= in_region.len() {
                return Err(Error::TopologyAcknowledgeWithoutVoters {
                    namespace: namespace.name.clone(),
                    region: region.to_owned(),
                    voters,
                });
            }
        }
    }
    Ok(())
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

    // --- The checked relations R1–R4. The views are what the session assembles
    // from the catalog rows; the relations themselves are pure, so the matrix
    // below is exact and store-free.

    /// The two-realm scheme WITHOUT the survival claim: R1's matrix stays R1's
    /// — the claim arithmetic is R2's own test, and a fixture that runs both
    /// would test whichever refuses first.
    fn bare() -> Topology {
        Topology::stated(
            vec![
                Realm::stated("dc-a".to_owned(), vec!["az-1".to_owned()])
                    .expect("a well-formed realm"),
                Realm::stated("dc-b".to_owned(), vec!["az-1".to_owned()])
                    .expect("a well-formed realm"),
            ],
            false,
        )
        .expect("a topology inside every structural relation")
    }

    fn member(
        row: &str,
        region: Option<&str>,
        domain: Option<&str>,
        coordinating: bool,
    ) -> MemberView {
        MemberView {
            row: row.to_owned(),
            region: region.map(str::to_owned),
            domain: domain.map(str::to_owned),
            coordinating,
            holds: MemberHolds::Store,
        }
    }

    fn namespace(id: u32, name: &str, copies: u32) -> NamespaceView {
        NamespaceView {
            id: NamespaceId::new(id),
            name: name.to_owned(),
            copies,
            acknowledge: None,
        }
    }

    #[test]
    fn r1_a_member_label_the_scheme_does_not_hold_is_refused_and_named() {
        let refused = check_topology(
            &bare(),
            &[member("n1", Some("dc-x"), None, true)],
            &[],
            None,
        )
        .expect_err("an orphan region parsed as a position");
        match refused {
            Error::TopologyOrphanLabel { row, label, what } => {
                assert_eq!(row, "n1");
                assert_eq!(label, "dc-x");
                assert_eq!(what, "region");
            }
            other => panic!("expected the orphan-label refusal, got {other}"),
        }

        // A domain the realm does not hold is refused the same way, named as a
        // domain — the label's kind is part of the direction.
        let refused = check_topology(
            &bare(),
            &[member("n1", Some("dc-a"), Some("az-9"), true)],
            &[],
            None,
        )
        .expect_err("an orphan domain parsed as a position");
        match refused {
            Error::TopologyOrphanLabel { label, what, .. } => {
                assert_eq!(label, "az-9");
                assert_eq!(what, "domain");
            }
            other => panic!("expected the orphan-label refusal, got {other}"),
        }

        // A member whose labels the scheme holds, and one with no labels at
        // all, are both fine: R1 checks the labels that exist, never invents
        // the ones that do not.
        check_topology(
            &bare(),
            &[
                member("n1", Some("dc-a"), Some("az-1"), true),
                member("n2", None, None, true),
            ],
            &[],
            None,
        )
        .expect("labels the scheme holds, and silence, both pass");
    }

    #[test]
    fn r2_the_claim_arithmetic_counts_voters_outside_each_realm() {
        let scheme = Topology::stated(
            vec![
                Realm::stated("dc-a".to_owned(), vec![]).expect("a well-formed realm"),
                Realm::stated("dc-b".to_owned(), vec![]).expect("a well-formed realm"),
                Realm::stated("dc-c".to_owned(), vec![]).expect("a well-formed realm"),
            ],
            true,
        )
        .expect("a topology inside every structural relation");
        // Two voters in one realm: losing it leaves two of four, which is not
        // more than half — the claim refuses, naming the realm and the
        // arithmetic.
        let members = [
            member("n1", Some("dc-a"), None, true),
            member("n2", Some("dc-a"), None, true),
            member("n3", Some("dc-b"), None, true),
            member("n4", Some("dc-c"), None, true),
        ];
        let refused = check_topology(&scheme, &members, &[], None)
            .expect_err("a claim the arithmetic breaks");
        match refused {
            Error::TopologyClaimBreaks { realm, left, total } => {
                assert_eq!(realm, "dc-a");
                assert_eq!(left, 2);
                assert_eq!(total, 4);
            }
            other => panic!("expected the claim refusal, got {other}"),
        }

        // One voter per realm survives any single loss: losing one leaves two
        // of three, which is more than half — the claim holds.
        let spread = [
            member("n1", Some("dc-a"), None, true),
            member("n2", Some("dc-b"), None, true),
            member("n3", Some("dc-c"), None, true),
        ];
        check_topology(&scheme, &spread, &[], None)
            .expect("one voter per realm survives any single loss");

        // A structure with no claim is not checked: silence is a third state.
        // The no-claim scheme declares the same three realms — the case is
        // about the claim's silence, and R1 refuses an undeclared label long
        // before R2's quiet is reachable.
        let quiet = Topology::stated(
            vec![
                Realm::stated("dc-a".to_owned(), vec![]).expect("a well-formed realm"),
                Realm::stated("dc-b".to_owned(), vec![]).expect("a well-formed realm"),
                Realm::stated("dc-c".to_owned(), vec![]).expect("a well-formed realm"),
            ],
            false,
        )
        .expect("a topology inside every structural relation");
        check_topology(&quiet, &members, &[], None).expect("no claim, no arithmetic");
    }

    #[test]
    fn r3_copies_of_a_kept_namespace_span_at_least_two_realms() {
        let namespaces = [namespace(1, "prod", 3)];
        // Both holders in one realm: the fiction refuses, naming the realm.
        let refused = check_topology(
            &bare(),
            &[
                member("n1", Some("dc-a"), None, true),
                member("n2", Some("dc-a"), None, true),
                member("n3", Some("dc-a"), None, true),
            ],
            &namespaces,
            None,
        )
        .expect_err("every copy in one realm read as spanning");
        match refused {
            Error::TopologyCopiesInOneRealm { namespace, realm } => {
                assert_eq!(namespace, "prod");
                assert_eq!(realm.as_deref(), Some("dc-a"));
            }
            other => panic!("expected the copies refusal, got {other}"),
        }

        // Copies across two realms hold; a single copy needs no span.
        check_topology(
            &bare(),
            &[
                member("n1", Some("dc-a"), None, true),
                member("n2", Some("dc-b"), None, true),
            ],
            &namespaces,
            None,
        )
        .expect("copies spanning realms hold");
        check_topology(
            &bare(),
            &[member("n1", Some("dc-a"), None, true)],
            &[namespace(1, "prod", 1)],
            None,
        )
        .expect("a single copy spans nothing and needs to");
    }

    #[test]
    fn r4_a_local_majority_namespace_needs_a_majority_where_the_leader_stands() {
        let mut local = namespace(1, "prod", 2);
        local.acknowledge = Some(tessari_types::Acknowledge::LocalMajority);
        let namespaces = [local];

        // The leader stands in dc-a with one voter there out of one — a
        // majority of one. The check holds.
        check_topology(
            &bare(),
            &[
                member("n1", Some("dc-a"), None, true),
                member("n2", Some("dc-b"), None, true),
            ],
            &namespaces,
            Some("dc-a"),
        )
        .expect("one voter in the leader's region is its own majority");

        // Two voters in the region, one coordinating: no majority, and the
        // refusal names the namespace, the region, and the count.
        let refused = check_topology(
            &bare(),
            &[
                member("n1", Some("dc-a"), None, true),
                member("n1b", Some("dc-a"), None, false),
                member("n2", Some("dc-b"), None, true),
            ],
            &namespaces,
            Some("dc-a"),
        )
        .expect_err("no majority in the leader's region");
        match refused {
            Error::TopologyAcknowledgeWithoutVoters {
                namespace,
                region,
                voters,
            } => {
                assert_eq!(namespace, "prod");
                assert_eq!(region, "dc-a");
                assert_eq!(voters, 1);
            }
            other => panic!("expected the acknowledge refusal, got {other}"),
        }

        // A leader naming no region is the write guard's case, not this
        // check's: the set-level check stays silent and lets the per-row
        // refusal name the moment a write actually asks.
        let single = [namespace(1, "prod", 1)];
        check_topology(
            &bare(),
            &[member("n1", Some("dc-a"), None, true)],
            &single,
            None,
        )
        .expect("an unlabelled leader is the write guard's case");
    }
}
