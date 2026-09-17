//! The failover policy the cluster runs under, and the pair that orders it.
//!
//! # Why a row rather than a file or a flag
//!
//! The periods in [`Failover`] decide how long a cluster waits before it treats
//! a leader as gone. Every node has to agree about them, because two nodes that
//! disagree about when a lease has expired are two nodes that can both believe
//! they may write — which is the split-brain the lease exists to prevent,
//! arriving through the mechanism meant to prevent it.
//!
//! A configuration file cannot carry that agreement. It is an **unreplicated
//! claim about a cluster-wide fact**, so two nodes holding different files is
//! not a conflict anything detects: each is internally consistent, each is
//! confident, and the disagreement is visible only in the outcome. A flag is the
//! same claim with a shorter life.
//!
//! A row is a log record. It is written by the lease-holding leader, ordered by
//! the log a majority agreed on, and applied by every follower through the path
//! every other record takes — so it needs no new record kind, no encoding
//! change and nothing new on the wire. That is
//! [`super::leadership`]'s own argument, and it is the same argument.
//!
//! # `(epoch, version)`, and why one of the two is not enough
//!
//! The **epoch** is the leadership the policy was written under. It settles the
//! case that matters: a policy written by a leader that has since been replaced
//! must not overwrite one written by its successor, however recently the old
//! leader wrote it. Ordering by arrival, or by a timestamp, gets that backwards
//! whenever a partitioned ex-leader reconnects.
//!
//! The **version** settles the case the epoch cannot: one leader changing the
//! policy twice within a single leadership. Both writes carry the same epoch, so
//! the epoch alone would make the second indistinguishable from the first and a
//! node would have no reason to prefer either.
//!
//! Neither field is a clock, and that is deliberate. A wall clock is the one
//! ordering two nodes can disagree about while both are working.
//!
//! # A higher pair installs; an equal or lower one is ignored
//!
//! Ignored rather than refused. A peer still running an older policy is not
//! misbehaving — it is behind, which is the ordinary state of a follower — and
//! an error there would turn a node that is catching up into a node that is
//! failing.

use std::collections::BTreeMap;
use std::time::Duration as Elapsed;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{Duration, Epoch, Number, RecordId, Value};

use super::definition::{count_of, object};
use super::{Catalog, system};
use crate::error::{Error, Result};
use crate::failover::Failover;

const FIELD_AWARENESS: &str = "awareness";
const FIELD_COLLECTION: &str = "collection";
const FIELD_ROUND: &str = "round";
const FIELD_CAMPAIGN: &str = "campaign";
const FIELD_LEASE: &str = "lease";
const FIELD_EPOCH: &str = "epoch";
const FIELD_VERSION: &str = "version";

const ENTITY: &str = "failover";

/// The one row this table holds.
///
/// A fixed handle rather than a generated id, because there is exactly one
/// policy per cluster: a second row would be a second answer to a question that
/// admits one, and nothing downstream would know which to read.
const ROW: &str = "policy";

/// Which policy this is, without the policy.
///
/// The pair alone, so it can travel where the periods must not. A peer greeting
/// carries this and never [`FailoverDefinition`]: the periods reach a node
/// through the log, because the row is a log record, and a second copy of them
/// on the wire would be a second spelling of the same configuration — which is
/// what this codebase has already paid for twice. The greeting's job is the
/// ORDERING, so the ordering is what it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailoverStamp {
    /// The leadership the policy was written under.
    pub epoch: Epoch,
    /// Which setting under that leadership this was.
    pub version: u64,
}

impl FailoverStamp {
    /// Whether this stamp supersedes `held`.
    ///
    /// The **one** spelling of the ordering, and it lives here rather than on
    /// the definition because the pair is the smaller thing: a caller holding
    /// only a stamp — a greeting reader — can ask, and
    /// [`FailoverDefinition::supersedes`] delegates so that a caller holding the
    /// whole row asks the same question through the same comparison. Two would
    /// agree for as long as nobody changed a policy twice inside one leadership,
    /// and would then disagree in the case the version field exists for, which
    /// is the shape of a bug nothing reports.
    #[must_use]
    pub fn supersedes(&self, held: &Self) -> bool {
        (self.epoch, self.version) > (held.epoch, held.version)
    }

    /// Whether this stamp supersedes what a node holds, when a node may hold
    /// nothing at all.
    ///
    /// `None` is a node running [`Failover::DEFAULT`] because nobody has ever
    /// set a policy, and **any** stamp supersedes it. That is the ordering and
    /// not a convenience: a node that has never been told is behind a node that
    /// has, and the alternative — treating *no policy* as unbeatable — would
    /// make the first policy a cluster ever sets the one nothing can act on.
    #[must_use]
    pub fn supersedes_held(&self, held: Option<&Self>) -> bool {
        held.is_none_or(|held| self.supersedes(held))
    }
}

/// A failover policy as the log records it: the periods, the leadership that set
/// them, and which setting under that leadership this was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailoverDefinition {
    /// The periods themselves, already checked against one another.
    ///
    /// A [`Failover`] in hand is one whose relations hold, because
    /// [`Failover::stated`] is the only way to build one that was not the
    /// default — so a row read back is re-checked at [`Self::from_value`]
    /// rather than trusted. A stored row is bytes on a disk, and bytes can be
    /// older than the relations, or edited.
    pub policy: Failover,
    /// The leadership this policy was written under.
    pub epoch: Epoch,
    /// Which setting under that leadership this was.
    pub version: u64,
}

impl FailoverDefinition {
    /// The record the policy is stored under.
    #[must_use]
    pub fn key() -> RecordId {
        RecordId::from(ROW)
    }

    /// Which policy this is, without the periods.
    #[must_use]
    pub fn stamp(&self) -> FailoverStamp {
        FailoverStamp {
            epoch: self.epoch,
            version: self.version,
        }
    }

    /// Whether this policy supersedes `held`.
    ///
    /// Asked of the stamps, so the comparison has one home. See
    /// [`FailoverStamp::supersedes`] for why one is the number that matters.
    #[must_use]
    pub fn supersedes(&self, held: &Self) -> bool {
        self.stamp().supersedes(&held.stamp())
    }

    /// The value written to the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the epoch, the version or a
    /// period is past `i64::MAX`, the widest integer a stored value holds.
    /// Refused rather than truncated, for [`super::leadership`]'s reason: a
    /// value that wrapped compares as older than the one before it, and every
    /// ordering rule in the cluster reads that comparison.
    pub fn to_value(&self) -> Result<Value> {
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: "number",
        };
        let count =
            |field: &'static str, value: u64| i64::try_from(value).map_err(|_| malformed(field));
        Ok(Value::Object(BTreeMap::from([
            (
                FIELD_AWARENESS.to_owned(),
                Value::Duration(stored(self.policy.awareness(), FIELD_AWARENESS)?),
            ),
            (
                FIELD_COLLECTION.to_owned(),
                Value::Duration(stored(self.policy.collection(), FIELD_COLLECTION)?),
            ),
            (
                FIELD_ROUND.to_owned(),
                Value::Duration(stored(self.policy.round(), FIELD_ROUND)?),
            ),
            (
                FIELD_CAMPAIGN.to_owned(),
                Value::Duration(stored(self.policy.campaign(), FIELD_CAMPAIGN)?),
            ),
            (
                FIELD_LEASE.to_owned(),
                Value::Duration(stored(self.policy.lease(), FIELD_LEASE)?),
            ),
            (
                FIELD_EPOCH.to_owned(),
                Value::Number(Number::Integer(count(FIELD_EPOCH, self.epoch.get())?)),
            ),
            (
                FIELD_VERSION.to_owned(),
                Value::Number(Number::Integer(count(FIELD_VERSION, self.version)?)),
            ),
        ])))
    }

    /// Read a definition back, re-checking the relations.
    ///
    /// Every field is required. The periods go back through
    /// [`Failover::stated`] rather than into the struct directly, so a row whose
    /// periods no longer hold together is refused at the boundary instead of
    /// becoming a policy nothing ever checked. That row can exist: it may have
    /// been written by a build whose relations differed, or edited.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type, and the relation's own refusal when the periods disagree.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, ENTITY)?;
        let malformed = |field: &'static str| Error::CatalogMalformed {
            entity: ENTITY,
            field,
            found: fields.get(field).map_or("none", Value::type_name),
        };
        let period = |field: &'static str| match fields.get(field) {
            Some(Value::Duration(held)) => elapsed(*held, field),
            _ => Err(malformed(field)),
        };
        let Some(epoch) = fields.get(FIELD_EPOCH) else {
            return Err(malformed(FIELD_EPOCH));
        };
        let Some(version) = fields.get(FIELD_VERSION) else {
            return Err(malformed(FIELD_VERSION));
        };
        Ok(Self {
            policy: Failover::stated(
                period(FIELD_AWARENESS)?,
                period(FIELD_COLLECTION)?,
                period(FIELD_ROUND)?,
                period(FIELD_CAMPAIGN)?,
                period(FIELD_LEASE)?,
            )?,
            epoch: Epoch::new(count_of(epoch, ENTITY, FIELD_EPOCH)?),
            version: count_of(version, ENTITY, FIELD_VERSION)?,
        })
    }
}

/// A period as the store spells durations.
fn stored(period: Elapsed, field: &'static str) -> Result<Duration> {
    let malformed = || Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: "duration",
    };
    let seconds = i64::try_from(period.as_secs()).map_err(|_| malformed())?;
    Duration::new(seconds, period.subsec_nanos()).ok_or_else(malformed)
}

/// And back again.
///
/// A negative period is refused rather than clamped. The store's duration type
/// is signed because an interval between two instants has a direction; a
/// *period a cluster waits* does not, and a negative one is a row that was
/// edited rather than written.
fn elapsed(period: Duration, field: &'static str) -> Result<Elapsed> {
    let malformed = || Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: "duration",
    };
    let seconds = u64::try_from(period.seconds()).map_err(|_| malformed())?;
    Ok(Elapsed::new(seconds, period.nanos()))
}

impl Catalog<'_, '_> {
    /// Record the policy this cluster is to run under.
    ///
    /// The caller supplies the pair. It is not derived here because the two
    /// fields answer to different owners: the epoch is the leadership the writer
    /// holds, and the version is one past whatever [`Catalog::failover`] last
    /// read — and a catalog method that invented either would be a second place
    /// the ordering is decided.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when the definition cannot be stored;
    /// see [`FailoverDefinition::to_value`].
    pub fn set_failover(
        &mut self,
        policy: Failover,
        epoch: Epoch,
        version: u64,
    ) -> Result<FailoverDefinition> {
        let definition = FailoverDefinition {
            policy,
            epoch,
            version,
        };
        self.transaction.put(
            system::address(system::FAILOVER, FailoverDefinition::key()),
            encode_payload(&definition.to_value()?).into_bytes(),
        );
        Ok(definition)
    }

    /// The policy the log says this cluster runs under, or `None` when nobody
    /// has set one.
    ///
    /// `None` is a real answer: a cluster nobody has configured runs
    /// [`Failover::DEFAULT`], and reporting that as *the default policy* here
    /// would make a node that was configured and a node that never was give the
    /// same answer to *what did the operator choose*.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored definition cannot be read.
    pub fn failover(&self) -> Result<Option<FailoverDefinition>> {
        let Some(payload) = self.transaction.get(&system::address(
            system::FAILOVER,
            FailoverDefinition::key(),
        ))?
        else {
            return Ok(None);
        };
        Ok(Some(FailoverDefinition::from_value(&decode_payload(
            &payload,
        )?)?))
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::{
        ENTITY, FIELD_AWARENESS, FIELD_CAMPAIGN, FIELD_COLLECTION, FIELD_EPOCH, FIELD_LEASE,
        FIELD_ROUND, FIELD_VERSION, FailoverDefinition,
    };
    use std::collections::BTreeMap;
    use std::time::Duration as Elapsed;
    use tessari_types::{Duration, Epoch, Number, Value};

    use crate::error::Error;
    use crate::failover::Failover;

    fn at(epoch: u64, version: u64) -> FailoverDefinition {
        FailoverDefinition {
            policy: Failover::DEFAULT,
            epoch: Epoch::new(epoch),
            version,
        }
    }

    #[test]
    fn a_policy_survives_the_round_trip_including_its_sub_second_remainder() {
        // A remainder is the part a seconds-only encoding would lose, and it
        // would lose it silently: the row would read back as a policy that
        // still satisfies every relation, just not the one that was written.
        let policy = Failover::stated(
            Elapsed::new(30, 500_000_000),
            Elapsed::from_secs(20),
            Elapsed::from_secs(3),
            Elapsed::from_secs(5),
            Elapsed::from_secs(40),
        )
        .unwrap();
        let written = FailoverDefinition {
            policy,
            epoch: Epoch::new(7),
            version: 2,
        };

        let read = FailoverDefinition::from_value(&written.to_value().unwrap()).unwrap();
        assert_eq!(read, written);
        assert_eq!(read.policy.awareness(), Elapsed::new(30, 500_000_000));
    }

    #[test]
    fn a_higher_pair_supersedes_and_an_equal_or_lower_one_does_not() {
        assert!(at(3, 0).supersedes(&at(2, 9)), "a newer leadership wins");
        assert!(
            at(2, 1).supersedes(&at(2, 0)),
            "a second setting under one leadership wins"
        );
        assert!(!at(2, 0).supersedes(&at(2, 0)), "an equal pair is ignored");
        assert!(
            !at(2, 0).supersedes(&at(2, 1)),
            "a lower version is ignored"
        );
        assert!(
            !at(2, 9).supersedes(&at(3, 0)),
            "a superseded leadership does not win on version — this is the \
             partitioned ex-leader reconnecting, and ordering by arrival or by \
             a clock gets it backwards"
        );
    }

    #[test]
    fn a_stored_row_whose_periods_no_longer_hold_together_is_refused_on_read() {
        // The row is built field by field rather than through `stated`,
        // because `stated` is what this is proving cannot be bypassed: a row
        // written by a build whose relations differed, or edited in place, is
        // refused at the boundary instead of becoming a policy nothing checked.
        let value = Value::Object(BTreeMap::from([
            (
                FIELD_AWARENESS.to_owned(),
                Value::Duration(Duration::from_seconds(10)),
            ),
            (
                FIELD_COLLECTION.to_owned(),
                Value::Duration(Duration::from_seconds(10)),
            ),
            (
                FIELD_ROUND.to_owned(),
                Value::Duration(Duration::from_seconds(1)),
            ),
            // Four times the round, where the window admits two.
            (
                FIELD_CAMPAIGN.to_owned(),
                Value::Duration(Duration::from_seconds(4)),
            ),
            (
                FIELD_LEASE.to_owned(),
                Value::Duration(Duration::from_seconds(10)),
            ),
            (FIELD_EPOCH.to_owned(), Value::Number(Number::Integer(1))),
            (FIELD_VERSION.to_owned(), Value::Number(Number::Integer(1))),
        ]));

        let refused = FailoverDefinition::from_value(&value)
            .expect_err("a row whose campaign outruns its window is not a policy");
        assert!(
            matches!(refused, Error::FailoverCampaignOutpaced { .. }),
            "the relation's own refusal, not a generic malformed-row error: the \
             row is well formed and the policy is not. Got {refused}"
        );
    }

    #[test]
    fn a_missing_field_is_named_rather_than_defaulted() {
        let mut fields = match at(1, 1).to_value().unwrap() {
            Value::Object(fields) => fields,
            other => panic!("a definition encodes as an object, got {other:?}"),
        };
        fields.remove(FIELD_VERSION);

        let refused = FailoverDefinition::from_value(&Value::Object(fields))
            .expect_err("a version that is absent is not a version of zero");
        match refused {
            Error::CatalogMalformed { entity, field, .. } => {
                assert_eq!(entity, ENTITY);
                assert_eq!(field, FIELD_VERSION);
            }
            other => panic!("expected a malformed-row refusal, got {other}"),
        }
    }

    #[test]
    fn a_period_stored_as_a_number_is_refused_rather_than_coerced() {
        // The store has a duration type, so a number here is a row somebody
        // else wrote. Coercing it would make the unit a convention instead of a
        // type, and the unit is the whole question.
        let mut fields = match at(1, 1).to_value().unwrap() {
            Value::Object(fields) => fields,
            other => panic!("a definition encodes as an object, got {other:?}"),
        };
        fields.insert(FIELD_LEASE.to_owned(), Value::Number(Number::Integer(10)));

        let refused =
            FailoverDefinition::from_value(&Value::Object(fields)).expect_err("ten of what?");
        match refused {
            Error::CatalogMalformed { field, found, .. } => {
                assert_eq!(field, FIELD_LEASE);
                assert_eq!(found, "number");
            }
            other => panic!("expected a malformed-row refusal, got {other}"),
        }
    }
}
