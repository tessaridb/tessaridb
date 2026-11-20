//! The load-balancing policy as the log records it, beside the failover row.
//!
//! For [`super::failover`]'s reason, one level down: the margins here decide
//! when serving work moves between nodes, which is a cluster-wide fact every
//! node must agree about, so the policy is a catalog row ordered by the same
//! `(epoch, version)` pair — a row written by the lease-holding leader, applied
//! by every follower through the path every other record takes.

use std::time::Duration as Elapsed;

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{Duration, Epoch, Number, RecordId, Value};

use super::definition::{count_of, object};
use super::{Catalog, system};
use crate::balancer::Balancer;
use crate::error::{Error, Result};

const FIELD_ABOVE: &str = "above";
const FIELD_WITHIN: &str = "within";
const FIELD_SPACING: &str = "spacing";
const FIELD_EPOCH: &str = "epoch";
const FIELD_VERSION: &str = "version";

const ENTITY: &str = "balancer";

/// The one row this table holds, for [`super::failover`]'s reason: exactly
/// one policy per store line, and a second row would be a second answer to a
/// question that admits one.
const ROW: &str = "policy";

/// A load-balancing policy as the log records it: the margins, the
/// leadership that set them, and which setting under that leadership this
/// was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalancerDefinition {
    /// The margins themselves, already checked against one another and
    /// against the failover policy they run beside.
    ///
    /// A [`Balancer`] in hand is one whose relations hold, because
    /// [`Balancer::stated`] is the only way to build one — so a row read back
    /// is re-checked at [`Self::from_value`] rather than trusted. A stored row
    /// is bytes on a disk, and bytes can be older than the relations, or
    /// edited.
    pub policy: Balancer,
    /// The leadership this policy was written under.
    pub epoch: Epoch,
    /// Which setting under that leadership this was.
    pub version: u64,
}

impl BalancerDefinition {
    /// The record the policy is stored under.
    #[must_use]
    pub fn key() -> RecordId {
        RecordId::Text(ROW.to_owned())
    }

    /// Whether this definition supersedes the one a node holds, when a node
    /// may hold nothing at all: the first policy a line ever sets supersedes
    /// the absence of one, the way the failover stamp's ordering reads it.
    #[must_use]
    pub fn supersedes_held(&self, held: Option<&Self>) -> bool {
        held.is_none_or(|held| (self.epoch, self.version) > (held.epoch, held.version))
    }

    /// The definition as a catalog record's value.
    ///
    /// # Errors
    ///
    /// Returns an error when a margin does not fit the store's spelling —
    /// which [`Balancer::stated`] already held, so this is the re-read's
    /// defence, not the write's.
    pub fn to_value(&self) -> Result<Value> {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            FIELD_ABOVE.to_owned(),
            Value::Number(Number::Integer(
                i64::try_from(self.policy.above_hundredths()).map_err(|_| {
                    Error::CatalogMalformed {
                        entity: ENTITY,
                        field: FIELD_ABOVE,
                        found: "a margin past any count",
                    }
                })?,
            )),
        );
        fields.insert(
            FIELD_WITHIN.to_owned(),
            Value::Duration(stored(self.policy.within(), FIELD_WITHIN)?),
        );
        fields.insert(
            FIELD_SPACING.to_owned(),
            Value::Duration(stored(self.policy.spacing(), FIELD_SPACING)?),
        );
        fields.insert(
            FIELD_EPOCH.to_owned(),
            Value::Number(Number::Integer(i64::try_from(self.epoch.get()).map_err(
                |_| Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_EPOCH,
                    found: "an epoch past any count",
                },
            )?)),
        );
        fields.insert(
            FIELD_VERSION.to_owned(),
            Value::Number(Number::Integer(i64::try_from(self.version).map_err(
                |_| Error::CatalogMalformed {
                    entity: ENTITY,
                    field: FIELD_VERSION,
                    found: "a count past any version",
                },
            )?)),
        );
        Ok(Value::Object(fields))
    }

    /// The definition a catalog record's value holds, re-checked rather than
    /// trusted.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is absent or malformed, and when the
    /// margins no longer hold together with the failover policy passed in —
    /// a row on disk can be older than the relations, or edited.
    pub fn from_value(value: &Value, failover: &crate::failover::Failover) -> Result<Self> {
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
        let Some(above) = fields.get(FIELD_ABOVE) else {
            return Err(malformed(FIELD_ABOVE));
        };
        let above = count_of(above, ENTITY, FIELD_ABOVE)?;
        Ok(Self {
            policy: Balancer::stated(
                above,
                period(FIELD_WITHIN)?,
                period(FIELD_SPACING)?,
                failover,
            )?,
            epoch: Epoch::new(count_of(
                fields
                    .get(FIELD_EPOCH)
                    .ok_or_else(|| malformed(FIELD_EPOCH))?,
                ENTITY,
                FIELD_EPOCH,
            )?),
            version: count_of(
                fields
                    .get(FIELD_VERSION)
                    .ok_or_else(|| malformed(FIELD_VERSION))?,
                ENTITY,
                FIELD_VERSION,
            )?,
        })
    }
}

/// A period as the store spells durations, for the signedness reason the
/// failover row gives: the store's duration type is signed because an
/// interval has a direction, and a *period a cluster waits* does not.
fn stored(period: Elapsed, field: &'static str) -> Result<Duration> {
    let malformed = || Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: "duration",
    };
    let seconds = i64::try_from(period.as_secs()).map_err(|_| malformed())?;
    Duration::new(seconds, period.subsec_nanos()).ok_or_else(malformed)
}

/// And back again. A negative period is refused rather than clamped, for the
/// failover row's reason: a negative one is a row that was edited rather than
/// written.
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
    /// Write the load-balancing policy, stamped with the leadership and the
    /// setting-under-it the pair orders by.
    pub fn set_balancer(
        &mut self,
        policy: Balancer,
        epoch: Epoch,
        version: u64,
    ) -> Result<BalancerDefinition> {
        let definition = BalancerDefinition {
            policy,
            epoch,
            version,
        };
        self.transaction.put(
            system::address(system::BALANCER, BalancerDefinition::key()),
            encode_payload(&definition.to_value()?).into_bytes(),
        );
        Ok(definition)
    }

    /// The load-balancing policy the log says this store line runs under, or
    /// `None` when nobody has set one — which is a real answer: the load pass
    /// is OFF until a statement turns it on, the way every automatic
    /// machinery here is.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored record cannot be read, and when the
    /// margins no longer hold together with the failover policy held — a row
    /// read back is re-checked rather than trusted.
    pub async fn balancer(&self) -> Result<Option<BalancerDefinition>> {
        let Some(payload) = self
            .transaction
            .get(&system::address(
                system::BALANCER,
                BalancerDefinition::key(),
            ))
            .await?
        else {
            return Ok(None);
        };
        let failover = self
            .failover()
            .await?
            .map_or(crate::failover::Failover::DEFAULT, |held| held.policy);
        BalancerDefinition::from_value(&decode_payload(&payload)?, &failover).map(Some)
    }

    /// Remove the load-balancing policy, answering whether one was held —
    /// the way [`super::topology`]'s drop answers, so a `DROP BALANCER`
    /// against a line holding none refuses with the same shape a `DROP
    /// TOPOLOGY` against a missing scheme does.
    pub async fn drop_balancer(&mut self) -> Result<bool> {
        let key = system::address(system::BALANCER, BalancerDefinition::key());
        if self.transaction.get(&key).await?.is_none() {
            return Ok(false);
        }
        self.transaction.delete(key);
        Ok(true)
    }
}
