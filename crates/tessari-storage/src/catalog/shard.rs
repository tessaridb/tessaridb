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

use std::collections::BTreeMap;

use tessari_types::{RecordId, ShardId, Value};

use tessari_types::IdentityKind;

use super::definition::{TableKind, TableShape, object};
use crate::error::{Error, Result};

const FIELD_ID: &str = "id";
const FIELD_FROM: &str = "from";

const ENTITY: &str = "shard";

/// A table's shards, in key order.
///
/// Never empty: a table that declared no split points has no map at all, which
/// is how an unsharded table reads, and one that declared `n` points has `n + 1`
/// shards. The first shard's span is open at the start and the last's at the
/// end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardMap {
    /// Ordered by lower bound; only the first has none.
    shards: Vec<Shard>,
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
        Ok(Some(Self { shards }))
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

    /// Whether this table has a shard numbered `id`.
    #[must_use]
    pub fn holds(&self, id: ShardId) -> bool {
        self.shards.iter().any(|shard| shard.id == id)
    }

    /// The value written into the table's definition.
    #[must_use]
    pub fn to_value(&self) -> Value {
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
        Ok(Self { shards })
    }
}

/// The map a table declaration asks for, or the refusal that says why not.
///
/// The rules live here, beside the map, rather than in the grammar: a parser is
/// the wrong place for an invariant about a stored table, because nothing stops
/// a later caller building a shape by hand.
///
/// # Errors
///
/// [`Error::SplitOnAKindThatIsNotRecords`] for any kind but a table or a
/// collection, [`Error::SplitNeedsGeneratedUuid`] for a counter identity, and
/// [`Error::SplitPointsOutOfOrder`] for points that do not ascend strictly.
pub(crate) fn declared_for(table: &str, shape: &TableShape) -> Result<Option<ShardMap>> {
    if shape.split.is_empty() {
        return Ok(None);
    }
    let kind = match &shape.kind {
        TableKind::Table | TableKind::Collection => None,
        TableKind::Bucket(_) => Some("a bucket"),
        TableKind::Edge(_) => Some("an edge table"),
        TableKind::Vector(_) => Some("a vector store"),
        TableKind::Geo => Some("a geo store"),
        TableKind::Vault(_) => Some("a vault"),
        TableKind::Queue(_) => Some("a queue"),
        TableKind::View(_) => Some("a view"),
        TableKind::Series(_) => Some("a series"),
    };
    if let Some(kind) = kind {
        return Err(Error::SplitOnAKindThatIsNotRecords {
            table: table.to_owned(),
            kind,
        });
    }
    if shape.identity != IdentityKind::Uuid {
        return Err(Error::SplitNeedsGeneratedUuid {
            table: table.to_owned(),
        });
    }
    ShardMap::declared(&shape.split).map_err(|refused| match refused {
        Unsplittable::OutOfOrder { position } => Error::SplitPointsOutOfOrder {
            table: table.to_owned(),
            position: position.saturating_add(1),
        },
        // A shard id is a u32; four billion split points is not a declaration
        // anybody writes, and it is refused under the ordering name because the
        // list, as written, does not describe a map.
        Unsplittable::TooMany => Error::SplitPointsOutOfOrder {
            table: table.to_owned(),
            position: shape.split.len(),
        },
    })
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
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn text(value: &str) -> RecordId {
        RecordId::Text(value.to_owned())
    }

    fn map(points: &[&str]) -> ShardMap {
        let points: Vec<_> = points.iter().map(|point| text(point)).collect();
        ShardMap::declared(&points).unwrap().unwrap()
    }

    #[test]
    fn no_points_is_no_map() {
        assert_eq!(ShardMap::declared(&[]).unwrap(), None);
    }

    #[test]
    fn two_points_make_three_shards_numbered_in_key_order() {
        let spans: Vec<_> = map(&["g", "p"]).spans().map(|span| span.id.get()).collect();
        assert_eq!(spans, vec![1, 2, 3]);
    }

    #[test]
    fn a_boundary_belongs_to_the_shard_it_begins() {
        let shards = map(&["g", "p"]);
        assert_eq!(shards.shard_of(&text("a")).get(), 1);
        assert_eq!(shards.shard_of(&text("f")).get(), 1);
        assert_eq!(
            shards.shard_of(&text("g")).get(),
            2,
            "a lower bound is inclusive"
        );
        assert_eq!(shards.shard_of(&text("o")).get(), 2);
        assert_eq!(shards.shard_of(&text("p")).get(), 3);
        assert_eq!(shards.shard_of(&text("zzz")).get(), 3);
    }

    #[test]
    fn identity_kinds_order_as_their_keys_do() {
        // Every integer sorts before every text, and every text before every
        // uuid — the discriminant order the key grammar fixes.
        let points = [RecordId::Int(100), text("m")];
        let shards = ShardMap::declared(&points).unwrap().unwrap();
        assert_eq!(shards.shard_of(&RecordId::Int(-5)).get(), 1);
        assert_eq!(shards.shard_of(&RecordId::Int(100)).get(), 2);
        assert_eq!(shards.shard_of(&text("a")).get(), 2);
        assert_eq!(shards.shard_of(&RecordId::Uuid([0; 16])).get(), 3);
    }

    #[test]
    fn points_out_of_order_or_repeated_are_named_not_sorted() {
        assert_eq!(
            ShardMap::declared(&[text("p"), text("g")]),
            Err(Unsplittable::OutOfOrder { position: 1 })
        );
        assert_eq!(
            ShardMap::declared(&[text("a"), text("g"), text("g")]),
            Err(Unsplittable::OutOfOrder { position: 2 })
        );
    }

    #[test]
    fn a_map_round_trips_through_the_value_the_catalog_holds() {
        let points = [
            RecordId::Int(7),
            text("m"),
            RecordId::Uuid([3; 16]),
            RecordId::Bytes(vec![1, 2]),
        ];
        let shards = ShardMap::declared(&points).unwrap().unwrap();
        assert_eq!(ShardMap::from_value(&shards.to_value()).unwrap(), shards);
    }

    #[test]
    fn spans_report_both_ends_with_the_open_ends_as_none() {
        let shards = map(&["g"]);
        let spans: Vec<_> = shards.spans().collect();
        assert_eq!(spans[0].from, None);
        assert_eq!(spans[0].to, Some(&text("g")));
        assert_eq!(spans[1].from, Some(&text("g")));
        assert_eq!(spans[1].to, None);
    }

    #[test]
    fn a_stored_map_that_is_not_one_map_is_refused_whole() {
        let bad = |value: Value| ShardMap::from_value(&value).is_err();
        assert!(bad(Value::Array(Vec::new())), "empty");
        let shard = |id: i64, from: Option<&str>| {
            let mut fields = BTreeMap::from([("id".to_owned(), Value::from(id))]);
            if let Some(from) = from {
                fields.insert("from".to_owned(), Value::from(from));
            }
            Value::Object(fields)
        };
        assert!(
            bad(Value::Array(vec![shard(1, Some("a"))])),
            "first has a bound"
        );
        assert!(
            bad(Value::Array(vec![shard(1, None), shard(2, None)])),
            "second has none"
        );
        assert!(
            bad(Value::Array(vec![
                shard(1, None),
                shard(2, Some("p")),
                shard(3, Some("g"))
            ])),
            "out of order"
        );
        assert!(
            bad(Value::Array(vec![shard(1, None), shard(1, Some("g"))])),
            "repeated id"
        );
        assert!(
            bad(Value::Array(vec![shard(0, None)])),
            "zero is not a shard"
        );
    }
}
