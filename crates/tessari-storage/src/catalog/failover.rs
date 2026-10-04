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
/// Written only when set, so a policy stored before the clause existed reads
/// back byte-identical (ADR-0113 D3).
const FIELD_BALANCE: &str = "balance_leaderships";

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
    /// Whether the store line's leader moves placements to even out the
    /// lines each node leads (`BALANCE LEADERSHIPS`, ADR-0113 D3).
    ///
    /// Beside the periods rather than inside [`Failover`]: it relates to none
    /// of them, and a policy value travels where this does not need to.
    pub balance_leaderships: bool,
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
        let mut fields = BTreeMap::from([
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
        ]);
        if self.balance_leaderships {
            fields.insert(FIELD_BALANCE.to_owned(), Value::Bool(true));
        }
        Ok(Value::Object(fields))
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
            balance_leaderships: match fields.get(FIELD_BALANCE) {
                None => false,
                Some(Value::Bool(balance)) => *balance,
                Some(_) => return Err(malformed(FIELD_BALANCE)),
            },
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
        balance_leaderships: bool,
    ) -> Result<FailoverDefinition> {
        let definition = FailoverDefinition {
            policy,
            epoch,
            version,
            balance_leaderships,
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
mod tests;
