//! Where a table's shards begin (G031, ADR-0080).
//!
//! A shard is a span of one table's identities, in the order a record key sorts
//! them. The map is part of the table's definition and is written with it, so
//! it replicates wherever the definition does and needs no record of its own.
//!
//! # A span never changes after it is declared
//!
//! That is what an id is worth. Every later reader of *which shard is this
//! record in* — the commit stamping it into the log, a stream filter, a
//! follower applying it — asks the same question of the same map and gets the
//! same answer, whenever it asks. A split, when one exists, retires an id and
//! mints two rather than moving a boundary, which keeps that true.
//!
//! # Identity order is the key order
//!
//! `RecordId`'s derived order is the order its key bytes sort in, which the
//! encoding crate asserts (`variant_order_on_disk_matches_variant_order_in_memory`).
//! A span over identities is therefore exactly a span over the table's keys, and
//! a boundary written `'g'` bounds the records a span read `t:'g'..` would walk.

mod declared;
mod moving;

pub(crate) use moving::Unmovable;

use std::collections::BTreeMap;

use tessari_types::{RecordId, ShardId, Value};

use tessari_types::IdentityKind;

use super::definition::{TableKind, TableShape, object};
use crate::error::{Error, Result};
pub(crate) use declared::declared_for;

const FIELD_ID: &str = "id";
const FIELD_FROM: &str = "from";
const FIELD_INTO: &str = "into";
const FIELD_VERSION: &str = "version";
const FIELD_SHARDS: &str = "shards";
const FIELD_RETIRED: &str = "retired";

const ENTITY: &str = "shard";

/// A table's shards, in key order.
///
/// Never empty: a table that declared no split points has no map at all, which
/// is how an unsharded table reads, and one that declared `n` points has `n + 1`
/// shards. The first shard's span is open at the start and the last's at the
/// end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardMap {
    /// How many splits and merges made this map; `0` for one as declared.
    version: u64,
    /// Ordered by lower bound; only the first has none.
    shards: Vec<Shard>,
    /// Shards a split or a merge retired, in the order they were retired.
    retired: Vec<Retired>,
}

/// One shard: its id and where its span begins.
///
/// The span ends where the next shard begins, so it is stored once rather than
/// twice. Two statements of one boundary are two things that can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shard {
    id: ShardId,
    from: Option<RecordId>,
}

/// A shard no write is filed in any more, and the shards that replaced it.
///
/// Its log still holds what was written before, which is why it is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Retired {
    id: ShardId,
    into: Vec<ShardId>,
}

/// One shard as a reader sees it: its id and both ends of its span.
///
/// `None` at an end is the open end — before every identity, or after every
/// one — and never a shard that holds nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSpan<'a> {
    /// The shard's id.
    pub id: ShardId,
    /// The first identity it holds, inclusive; `None` is the start of the table.
    pub from: Option<&'a RecordId>,
    /// The first identity it does NOT hold; `None` is the end of the table.
    pub to: Option<&'a RecordId>,
}

/// Why a declared list of split points does not describe one shard map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unsplittable {
    /// The point at this position does not sort strictly after the one before
    /// it — out of order, or a repeat.
    OutOfOrder {
        /// The offending point, zero-based in written order.
        position: usize,
    },
    /// More points than a shard id can number.
    TooMany,
}

impl ShardMap {
    /// The map `SPLIT AT points` declares, or `None` for no points at all.
    ///
    /// Ids run from `1` in key order. Points must already be in strictly
    /// ascending order: a map built by sorting them would quietly accept a list
    /// its author wrote wrong, and the refusal names which point is out of place.
    pub(crate) fn declared(points: &[RecordId]) -> std::result::Result<Option<Self>, Unsplittable> {
        let Some(first) = points.first() else {
            return Ok(None);
        };
        let mut previous = first;
        for (position, point) in points.iter().enumerate().skip(1) {
            if point <= previous {
                return Err(Unsplittable::OutOfOrder { position });
            }
            previous = point;
        }
        let mut shards = Vec::with_capacity(points.len().saturating_add(1));
        shards.push(Shard {
            id: ShardId::new(1),
            from: None,
        });
        for (offset, point) in points.iter().enumerate() {
            let id = u32::try_from(offset)
                .ok()
                .and_then(|offset| offset.checked_add(2))
                .ok_or(Unsplittable::TooMany)?;
            shards.push(Shard {
                id: ShardId::new(id),
                from: Some(point.clone()),
            });
        }
        Ok(Some(Self {
            version: 0,
            shards,
            retired: Vec::new(),
        }))
    }

    /// The shard holding `id`.
    ///
    /// Total: every identity falls in exactly one span, because the first span
    /// is open at the start and each one ends where the next begins.
    #[must_use]
    pub fn shard_of(&self, id: &RecordId) -> ShardId {
        // The number of shards whose lower bound is at or below `id`; the first
        // is always counted, because its bound is the open start.
        let at_or_below = self
            .shards
            .partition_point(|shard| shard.from.as_ref().is_none_or(|from| from <= id));
        self.shards
            .get(at_or_below.saturating_sub(1))
            .map_or(ShardId::new(1), |shard| shard.id)
    }

    /// Every shard with both ends of its span, in key order.
    pub fn spans(&self) -> impl Iterator<Item = ShardSpan<'_>> {
        self.shards
            .iter()
            .enumerate()
            .map(|(position, shard)| ShardSpan {
                id: shard.id,
                from: shard.from.as_ref(),
                to: self
                    .shards
                    .get(position.saturating_add(1))
                    .and_then(|next| next.from.as_ref()),
            })
    }

    /// Whether this table has a shard numbered `id`, live or retired.
    ///
    /// A retired shard is still the table's: its log holds what was written to
    /// it, so a subscription or a placement may still name it.
    #[must_use]
    pub fn holds(&self, id: ShardId) -> bool {
        self.logs().any(|held| held == id)
    }

    /// How many splits and merges made this map.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// The shards that replaced `id`, or nothing when it is not retired.
    #[must_use]
    pub fn successors(&self, id: ShardId) -> Vec<ShardId> {
        self.retired
            .iter()
            .find(|retired| retired.id == id)
            .map(|retired| retired.into.clone())
            .unwrap_or_default()
    }

    /// Every retired shard and the shards that replaced it, in the order they
    /// were retired.
    pub fn retired(&self) -> impl Iterator<Item = (ShardId, &[ShardId])> {
        self.retired
            .iter()
            .map(|retired| (retired.id, retired.into.as_slice()))
    }

    /// Every shard whose log may hold records — live and retired — by id.
    pub fn logs(&self) -> impl Iterator<Item = ShardId> {
        let mut ids: Vec<ShardId> = self
            .shards
            .iter()
            .map(|shard| shard.id)
            .chain(self.retired.iter().map(|retired| retired.id))
            .collect();
        ids.sort_unstable();
        ids.into_iter()
    }

    /// The value written into the table's definition.
    ///
    /// A map no statement has moved is the bare list it always was, so every
    /// table declared before splits existed keeps its stored bytes; a moved map
    /// carries its version and its retired shards beside the list.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let spans = self.spans_value();
        if self.version == 0 {
            return spans;
        }
        let retired = self
            .retired
            .iter()
            .map(|retired| {
                Value::Object(BTreeMap::from([
                    (
                        FIELD_ID.to_owned(),
                        Value::from(i64::from(retired.id.get())),
                    ),
                    (
                        FIELD_INTO.to_owned(),
                        Value::Array(
                            retired
                                .into
                                .iter()
                                .map(|id| Value::from(i64::from(id.get())))
                                .collect(),
                        ),
                    ),
                ]))
            })
            .collect();
        Value::Object(BTreeMap::from([
            (
                FIELD_VERSION.to_owned(),
                Value::from(i64::try_from(self.version).unwrap_or(i64::MAX)),
            ),
            (FIELD_SHARDS.to_owned(), spans),
            (FIELD_RETIRED.to_owned(), Value::Array(retired)),
        ]))
    }

    /// The live shards as the stored list.
    fn spans_value(&self) -> Value {
        Value::Array(
            self.shards
                .iter()
                .map(|shard| {
                    let mut fields = BTreeMap::from([(
                        FIELD_ID.to_owned(),
                        Value::from(i64::from(shard.id.get())),
                    )]);
                    if let Some(from) = &shard.from {
                        fields.insert(FIELD_FROM.to_owned(), id_to_value(from));
                    }
                    Value::Object(fields)
                })
                .collect(),
        )
    }

    /// Read a map back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] for anything that is not the map
    /// [`Self::to_value`] writes: an empty list, a first shard with a lower
    /// bound or a later one without, bounds out of order, or a repeated id. A
    /// map this build cannot read whole is refused rather than read in part,
    /// because a record routed by half a map is routed wrongly with no error.
    pub fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields) = value else {
            return Self::spans_from(value);
        };
        let version = match fields.get(FIELD_VERSION) {
            Some(Value::Number(number)) => number
                .as_exact_integer()
                .and_then(|raw| u64::try_from(raw).ok())
                .filter(|raw| *raw > 0)
                .ok_or_else(|| malformed(FIELD_VERSION, "a number that is not a version"))?,
            _ => return Err(malformed(FIELD_VERSION, "none")),
        };
        let mut map = Self::spans_from(
            fields
                .get(FIELD_SHARDS)
                .ok_or_else(|| malformed(FIELD_SHARDS, "none"))?,
        )?;
        let Some(Value::Array(entries)) = fields.get(FIELD_RETIRED) else {
            return Err(malformed(FIELD_RETIRED, "not a list"));
        };
        for entry in entries {
            let fields = object(entry, ENTITY)?;
            let id = fields
                .get(FIELD_ID)
                .and_then(shard_id)
                .ok_or_else(|| malformed(FIELD_ID, "a number that is not a shard id"))?;
            let Some(Value::Array(into)) = fields.get(FIELD_INTO) else {
                return Err(malformed(FIELD_INTO, "not a list"));
            };
            let into: Vec<ShardId> = into
                .iter()
                .map(|each| {
                    shard_id(each)
                        .ok_or_else(|| malformed(FIELD_INTO, "a number that is not a shard id"))
                })
                .collect::<Result<_>>()?;
            if into.is_empty() || map.holds(id) {
                return Err(malformed(
                    FIELD_RETIRED,
                    "a retired shard that is live, repeated or replaced by nothing",
                ));
            }
            map.retired.push(Retired { id, into });
        }
        map.version = version;
        Ok(map)
    }

    /// Read the stored list of live shards.
    fn spans_from(value: &Value) -> Result<Self> {
        let Value::Array(entries) = value else {
            return Err(malformed("shards", "not a list"));
        };
        let mut shards: Vec<Shard> = Vec::with_capacity(entries.len());
        for (position, entry) in entries.iter().enumerate() {
            let fields = object(entry, ENTITY)?;
            let id = match fields.get(FIELD_ID) {
                Some(Value::Number(number)) => number
                    .as_exact_integer()
                    .and_then(|raw| u32::try_from(raw).ok())
                    .filter(|raw| *raw > 0)
                    .map(ShardId::new)
                    .ok_or_else(|| malformed(FIELD_ID, "a number that is not a shard id"))?,
                _ => return Err(malformed(FIELD_ID, "none")),
            };
            let from = match fields.get(FIELD_FROM) {
                None => None,
                Some(held) => Some(
                    id_from_value(held)
                        .ok_or_else(|| malformed(FIELD_FROM, "a value that is not an identity"))?,
                ),
            };
            let in_place = match (position, &from, shards.last()) {
                (0, None, _) => true,
                (
                    _,
                    Some(bound),
                    Some(Shard {
                        from: Some(before), ..
                    }),
                ) => bound > before,
                (_, Some(_), Some(Shard { from: None, .. })) => true,
                _ => false,
            };
            if !in_place {
                return Err(malformed(FIELD_FROM, "a bound out of key order"));
            }
            if shards.iter().any(|held| held.id == id) {
                return Err(malformed(FIELD_ID, "a repeated shard id"));
            }
            shards.push(Shard { id, from });
        }
        if shards.is_empty() {
            return Err(malformed("shards", "an empty list"));
        }
        Ok(Self {
            version: 0,
            shards,
            retired: Vec::new(),
        })
    }
}

/// A stored shard id: a positive whole number that fits one.
fn shard_id(value: &Value) -> Option<ShardId> {
    let Value::Number(number) = value else {
        return None;
    };
    number
        .as_exact_integer()
        .and_then(|raw| u32::try_from(raw).ok())
        .filter(|raw| *raw > 0)
        .map(ShardId::new)
}

fn malformed(field: &'static str, found: &'static str) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found,
    }
}

/// An identity as a stored value — one value kind per identity kind, so the
/// four never collide on the way back.
fn id_to_value(id: &RecordId) -> Value {
    match id {
        RecordId::Int(value) => Value::from(*value),
        RecordId::Text(value) => Value::from(value.as_str()),
        RecordId::Uuid(bytes) => Value::Uuid(*bytes),
        RecordId::Bytes(bytes) => Value::Bytes(bytes.clone()),
    }
}

fn id_from_value(value: &Value) -> Option<RecordId> {
    match value {
        Value::Number(number) => number.as_exact_integer().map(RecordId::Int),
        Value::String(text) => Some(RecordId::Text(text.clone())),
        Value::Uuid(bytes) => Some(RecordId::Uuid(*bytes)),
        Value::Bytes(bytes) => Some(RecordId::Bytes(bytes.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
