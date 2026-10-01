//! Splitting a shard and merging two, while the table serves (ADR-0095).
//!
//! # A change retires ids and mints new ones
//!
//! No span is ever edited. A split retires the shard holding the point and mints
//! two that cover what it covered; a merge retires two neighbours and mints one.
//! Every reader that asked *which shard is this record in* before the change
//! keeps a true answer about what it read — the retired shard's log still holds
//! what was written to it — and every write after the change is filed under an
//! id that never meant anything else.
//!
//! # The map counts its changes
//!
//! `version` is how two holders of one table's map tell whether they hold the
//! same one without comparing every span, and the retired list is how a
//! subscription or a placement naming an old id is carried to the ids that
//! replaced it.

use tessari_types::{RecordId, ShardId};

use super::{Retired, Shard, ShardMap};

/// Why a split or a merge does not describe a new map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unmovable {
    /// The point already begins a shard: splitting there would mint an empty one.
    OnABoundary,
    /// The shard is not one of the table's live shards — unknown, or retired.
    NotLive(ShardId),
    /// Two shards that do not touch, or one shard named twice.
    NotAdjacent,
    /// No id or version is left to mint.
    Exhausted,
}

impl ShardMap {
    /// The map after splitting the shard that holds `point` at `point`.
    ///
    /// The holder is retired and two shards are minted in its place: the first
    /// begins where the holder began and the second at `point`.
    pub(crate) fn split_at(&self, point: &RecordId) -> Result<Self, Unmovable> {
        let holder = self.shard_of(point);
        let position = self.position(holder).ok_or(Unmovable::NotLive(holder))?;
        let begins = self
            .shards
            .get(position)
            .and_then(|shard| shard.from.clone());
        if begins.as_ref() == Some(point) {
            return Err(Unmovable::OnABoundary);
        }
        let [left, right] = self.mint::<2>()?;
        let mut moved = self.next()?;
        moved.shards.splice(
            position..=position,
            [
                Shard {
                    id: left,
                    from: begins,
                },
                Shard {
                    id: right,
                    from: Some(point.clone()),
                },
            ],
        );
        moved.retired.push(Retired {
            id: holder,
            into: vec![left, right],
        });
        Ok(moved)
    }

    /// The map after merging the adjacent live shards `first` and `second`,
    /// named in either order.
    pub(crate) fn merged(&self, first: ShardId, second: ShardId) -> Result<Self, Unmovable> {
        let one = self.position(first).ok_or(Unmovable::NotLive(first))?;
        let other = self.position(second).ok_or(Unmovable::NotLive(second))?;
        let (lower, upper) = (one.min(other), one.max(other));
        if lower.checked_add(1) != Some(upper) {
            return Err(Unmovable::NotAdjacent);
        }
        let begins = self.shards.get(lower).and_then(|shard| shard.from.clone());
        let retiring: Vec<ShardId> = self
            .shards
            .get(lower..=upper)
            .map(|pair| pair.iter().map(|shard| shard.id).collect())
            .unwrap_or_default();
        let [joined] = self.mint::<1>()?;
        let mut moved = self.next()?;
        moved.shards.splice(
            lower..=upper,
            [Shard {
                id: joined,
                from: begins,
            }],
        );
        moved.retired.extend(retiring.into_iter().map(|id| Retired {
            id,
            into: vec![joined],
        }));
        Ok(moved)
    }

    /// Where the live shard `id` sits, in key order.
    fn position(&self, id: ShardId) -> Option<usize> {
        self.shards.iter().position(|shard| shard.id == id)
    }

    /// This map with its version advanced, for a change to be made to.
    fn next(&self) -> Result<Self, Unmovable> {
        let mut moved = self.clone();
        // Bounded by what the stored value can carry, so a version is never
        // written as something other than itself.
        moved.version = self
            .version
            .checked_add(1)
            .filter(|next| i64::try_from(*next).is_ok())
            .ok_or(Unmovable::Exhausted)?;
        Ok(moved)
    }

    /// `N` ids past every id this table has ever used, live or retired.
    fn mint<const N: usize>(&self) -> Result<[ShardId; N], Unmovable> {
        let highest = self.logs().map(ShardId::get).max().unwrap_or(0);
        let mut minted = [ShardId::new(1); N];
        let mut next = highest;
        for slot in &mut minted {
            next = next.checked_add(1).ok_or(Unmovable::Exhausted)?;
            *slot = ShardId::new(next);
        }
        Ok(minted)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    fn text(value: &str) -> RecordId {
        RecordId::Text(value.to_owned())
    }

    fn map(points: &[&str]) -> ShardMap {
        let points: Vec<_> = points.iter().map(|point| text(point)).collect();
        ShardMap::declared(&points).unwrap().unwrap()
    }

    fn live(map: &ShardMap) -> Vec<(u32, Option<String>)> {
        map.spans()
            .map(|span| {
                let from = span.from.map(|from| match from {
                    RecordId::Text(text) => text.clone(),
                    other => format!("{other:?}"),
                });
                (span.id.get(), from)
            })
            .collect()
    }

    #[test]
    fn a_split_retires_the_holder_and_mints_two_past_every_id_used() {
        let split = map(&["g", "p"]).split_at(&text("m")).unwrap();
        assert_eq!(
            live(&split),
            vec![
                (1, None),
                (4, Some("g".to_owned())),
                (5, Some("m".to_owned())),
                (3, Some("p".to_owned())),
            ]
        );
        assert_eq!(split.version(), 1);
        assert_eq!(
            split.successors(ShardId::new(2)),
            vec![ShardId::new(4), ShardId::new(5)]
        );
        assert!(
            split.holds(ShardId::new(2)),
            "a retired shard's log still holds records"
        );
        assert_eq!(split.shard_of(&text("k")).get(), 4);
        assert_eq!(split.shard_of(&text("n")).get(), 5);
    }

    #[test]
    fn ids_are_never_reused_after_a_merge_retires_the_highest() {
        let merged = map(&["g", "p"])
            .merged(ShardId::new(2), ShardId::new(3))
            .unwrap();
        assert_eq!(live(&merged), vec![(1, None), (4, Some("g".to_owned()))]);
        assert_eq!(merged.version(), 1);
        let again = merged.split_at(&text("x")).unwrap();
        assert_eq!(
            live(&again),
            vec![
                (1, None),
                (5, Some("g".to_owned())),
                (6, Some("x".to_owned()))
            ],
            "4 was minted by the merge and 2 and 3 retired by it; none of them comes back"
        );
        assert_eq!(again.version(), 2);
    }

    #[test]
    fn a_merge_takes_its_shards_in_either_order() {
        let backwards = map(&["g", "p"])
            .merged(ShardId::new(3), ShardId::new(2))
            .unwrap();
        assert_eq!(live(&backwards), vec![(1, None), (4, Some("g".to_owned()))]);
        assert_eq!(
            Ok(backwards),
            map(&["g", "p"]).merged(ShardId::new(2), ShardId::new(3))
        );
    }

    #[test]
    fn what_cannot_be_moved_is_named() {
        let shards = map(&["g", "p"]);
        assert_eq!(shards.split_at(&text("g")), Err(Unmovable::OnABoundary));
        assert_eq!(
            shards.merged(ShardId::new(1), ShardId::new(3)),
            Err(Unmovable::NotAdjacent)
        );
        assert_eq!(
            shards.merged(ShardId::new(2), ShardId::new(2)),
            Err(Unmovable::NotAdjacent)
        );
        assert_eq!(
            shards.merged(ShardId::new(2), ShardId::new(9)),
            Err(Unmovable::NotLive(ShardId::new(9)))
        );
        let split = shards.split_at(&text("m")).unwrap();
        assert_eq!(
            split.merged(ShardId::new(2), ShardId::new(4)),
            Err(Unmovable::NotLive(ShardId::new(2))),
            "a retired shard is not merged again"
        );
    }

    #[test]
    fn a_moved_map_round_trips_and_an_unmoved_one_keeps_its_old_bytes() {
        let declared = map(&["g", "p"]);
        assert!(
            matches!(declared.to_value(), tessari_types::Value::Array(_)),
            "a map no statement has moved is stored exactly as before"
        );
        let moved = declared
            .split_at(&text("m"))
            .unwrap()
            .merged(ShardId::new(5), ShardId::new(3))
            .unwrap();
        assert_eq!(ShardMap::from_value(&moved.to_value()).unwrap(), moved);
        assert_eq!(moved.version(), 2);
        let logs: Vec<u32> = moved.logs().map(ShardId::get).collect();
        assert_eq!(logs, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_stored_moved_map_that_contradicts_itself_is_refused() {
        use tessari_types::Value;
        let moved = map(&["g"]).split_at(&text("m")).unwrap();
        let Value::Object(mut fields) = moved.to_value() else {
            panic!("a moved map is stored as an object");
        };
        let mut live_and_retired = fields.clone();
        live_and_retired.insert(
            "retired".to_owned(),
            Value::Array(vec![Value::Object(std::collections::BTreeMap::from([
                ("id".to_owned(), Value::from(1_i64)),
                ("into".to_owned(), Value::Array(vec![Value::from(3_i64)])),
            ]))]),
        );
        assert!(
            ShardMap::from_value(&Value::Object(live_and_retired)).is_err(),
            "shard 1 is live and cannot also be retired"
        );
        fields.insert("version".to_owned(), Value::from(0_i64));
        assert!(
            ShardMap::from_value(&Value::Object(fields)).is_err(),
            "a moved map has a version past zero"
        );
    }
}
