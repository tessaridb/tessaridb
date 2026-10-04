#![allow(clippy::unwrap_used)]

use core::ops::Bound;

use tessari_geo::{Cell, ORDER, Snapped};
use tessari_kv::{Key, KeyRange};
use tessari_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

use super::{Bounds, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue};
use crate::error::Error;
use crate::index_keys::{INDEX_PREFIX_LEN, IndexAddress};
use crate::kind::KeyKind;

fn address() -> IndexAddress {
    IndexAddress::new(
        NamespaceId::new(3),
        DatabaseId::new(4),
        TableId::new(5),
        IndexId::new(6),
    )
}

fn at(longitude: i64, latitude: i64) -> Snapped {
    Snapped::from_units(longitude, latitude).unwrap()
}

/// A deterministic sequence, so a failure is reproducible from its own output.
struct Spread(u64);

impl Spread {
    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state.wrapping_shl(13);
        state ^= state.wrapping_shr(7);
        state ^= state.wrapping_shl(17);
        self.0 = state;
        state
    }
}

/// The cell of `position` at `level`.
fn cell_at(position: Snapped, level: u32) -> Cell {
    let finest = Cell::containing(position);
    Cell::starting_at(level, coarsen(finest.range().0, level)).unwrap()
}

/// Whether a span takes in a key.
///
/// Written here rather than taken from [`KeyRange`], deliberately: the span
/// under test would otherwise be asked whether it contains something using
/// its own idea of containment, which is the encoder agreeing with itself.
/// This says what a backend does — start inclusive, end exclusive — in the
/// one form the tests below need.
fn holds(span: &KeyRange, key: &Key) -> bool {
    let after_start = match span.start() {
        Bound::Included(low) => key >= low,
        Bound::Excluded(low) => key > low,
        Bound::Unbounded => true,
    };
    let before_end = match span.end() {
        Bound::Included(high) => key <= high,
        Bound::Excluded(high) => key < high,
        Bound::Unbounded => true,
    };
    after_start && before_end
}

/// The start of the range of the level-`level` cell holding `first`.
fn coarsen(first: u64, level: u32) -> u64 {
    let width = 2_u32.saturating_mul(ORDER.saturating_sub(level));
    if width >= u64::BITS {
        return 0;
    }
    first
        .checked_shr(width)
        .and_then(|index| index.checked_shl(width))
        .unwrap_or(0)
}

#[test]
fn an_entry_survives_the_round_trip_at_every_level() {
    let mut spread = Spread(0x5a71_a100);
    for up in 0..=ORDER {
        let position = at(
            i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
            i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
        );
        let key = SpatialIndexKey::new(
            address(),
            cell_at(position, ORDER.saturating_sub(up)),
            RecordId::from("r"),
        );
        let encoded = key.encode();
        assert_eq!(
            SpatialIndexKey::decode(encoded.as_slice()).unwrap(),
            key,
            "an entry {up} level(s) above the finest should read back unchanged"
        );
    }
}

#[test]
fn an_entry_at_any_level_sorts_inside_the_run_of_every_cell_above_it() {
    // The property the whole layout exists for, and the one a query side
    // will rest on: an entry must be findable by scanning the byte range of
    // any coarser cell containing it, **whatever level the entry sits at**.
    //
    // The levels are the load-bearing part of the generator. A version of
    // this test that only placed entries at the finest level passed with the
    // level byte moved in front of the range start — every key then carried
    // the same leading byte, so its position could not matter and the
    // assertion never reached the case it exists for. A real covering mixes
    // levels, and mixed levels are what the ordering has to survive.
    //
    // The bounds are built from `to_be_bytes` rather than through the key
    // writer, so the comparison is a second statement of the layout rather
    // than the encoder agreeing with itself. They deliberately stop after
    // the range start: the level byte and the record id both follow, and a
    // bound that pinned a level would exclude exactly the coarser entries
    // this test is about.
    let mut spread = Spread(0x0d0e_5c3d);
    for _ in 0..32 {
        let position = at(
            i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
            i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
        );
        for down in 0..=ORDER {
            let entry =
                SpatialIndexKey::new(address(), cell_at(position, down), RecordId::from("r"))
                    .encode();
            // Coarser is a *lower* level, so an entry at `down` has its
            // ancestors below it and not above it in the numbering.
            for up in 0..down {
                let (first, last) = cell_at(position, up).range();
                let mut low = address().prefix(KeyKind::SpatialIndex);
                low.extend_from_slice(&first.to_be_bytes());
                let mut high = address().prefix(KeyKind::SpatialIndex);
                high.extend_from_slice(&last.to_be_bytes());
                high.extend_from_slice(&[u8::MAX; 9]);
                assert!(
                    low.as_slice() <= entry.as_slice() && entry.as_slice() <= high.as_slice(),
                    "a level-{down} entry should sort inside the run of the level-{up} cell above it"
                );
            }
        }
    }
}

#[test]
fn the_descendant_span_takes_in_every_entry_below_a_cell_and_stops_there() {
    // The scan half of a lookup, from both sides at once: every entry at or
    // below the cell must fall inside the span, and an entry under the cell
    // *next* to it must fall outside. Only the second half can fail when the
    // upper bound is wrong, and only the first when the lower one is — a
    // test asserting either alone passes with the span wide open.
    let mut spread = Spread(0x5c11_0d0e);
    for _ in 0..16 {
        let position = at(
            i64::try_from(spread.next() % 360_000_000_001).unwrap_or(0) - 180_000_000_000,
            i64::try_from(spread.next() % 180_000_000_001).unwrap_or(0) - 90_000_000_000,
        );
        for level in 1..=ORDER {
            let cell = cell_at(position, level);
            let span = SpatialIndexKey::descendants(&address(), cell);
            for below in level..=ORDER {
                let entry =
                    SpatialIndexKey::new(address(), cell_at(position, below), RecordId::from("r"))
                        .encode();
                assert!(
                    holds(&span, &entry),
                    "a level-{below} entry should be inside the span of the level-{level} \
                         cell above it"
                );
            }
            // The entry immediately past the cell's own run. It belongs to
            // whatever cell begins there, which is never this one.
            let (_, last) = cell.range();
            if let Some(past) = last.checked_add(1) {
                let outside = SpatialIndexKey::new(
                    address(),
                    Cell::starting_at(ORDER, past).expect("a finest cell begins at every number"),
                    RecordId::from("r"),
                )
                .encode();
                assert!(
                    !holds(&span, &outside),
                    "the first entry past a level-{level} cell's run should be outside its span"
                );
            }
        }
    }
}

#[test]
fn the_root_span_is_the_whole_index_and_no_more() {
    // The one cell whose run reaches the end of the numbering, so the
    // successor its upper bound would need does not exist. Getting this
    // wrong gives an empty span, and an empty span answers every
    // world-scale query with nothing.
    let span = SpatialIndexKey::descendants(&address(), Cell::root());
    for level in [0_u32, 1, 16, ORDER] {
        let entry = SpatialIndexKey::new(
            address(),
            Cell::starting_at(level, 0).expect("every level begins at zero"),
            RecordId::from("r"),
        )
        .encode();
        assert!(
            holds(&span, &entry),
            "a level-{level} entry is under the root"
        );
    }
    let farthest = SpatialIndexKey::new(
        address(),
        Cell::starting_at(ORDER, u64::MAX).expect("the last finest cell"),
        RecordId::from("r"),
    )
    .encode();
    assert!(holds(&span, &farthest));
    // And it stops at this index: a neighbouring index's entries are not the
    // root's descendants, however the root's own bound is written.
    let elsewhere = SpatialIndexKey::new(
        IndexAddress::new(
            NamespaceId::new(3),
            DatabaseId::new(4),
            TableId::new(5),
            IndexId::new(7),
        ),
        Cell::root(),
        RecordId::from("r"),
    )
    .encode();
    assert!(!holds(&span, &elsewhere));
}

#[test]
fn a_cell_prefix_names_one_cell_and_not_the_one_a_level_away() {
    // The lookup half. The prefix has to pin the level as well as the start,
    // because a coarse cell and the fine cell at its own first corner share
    // a range start — they differ only in the byte after it.
    let position = at(2_350_000_000, 48_850_000_000);
    for level in 1..ORDER {
        let cell = cell_at(position, level);
        let prefix = SpatialIndexKey::cell_prefix(&address(), cell);
        let here = SpatialIndexKey::new(address(), cell, RecordId::from("r")).encode();
        assert!(here.as_slice().starts_with(&prefix));
        let finer = cell_at(position, level.saturating_add(1));
        let there = SpatialIndexKey::new(address(), finer, RecordId::from("r")).encode();
        assert!(
            !there.as_slice().starts_with(&prefix),
            "a level-{level} prefix should not take in the level below it"
        );
    }
}

#[test]
fn a_level_and_a_start_that_name_no_cell_are_refused() {
    // What a corrupt key looks like: the level says the cell is coarse, the
    // start says it began somewhere no cell of that level does. Accepting it
    // would hand a reader a square nothing ever wrote.
    let key = SpatialIndexKey::new(
        address(),
        Cell::starting_at(ORDER, 7).unwrap(),
        RecordId::from("r"),
    );
    let mut bytes = key.encode().as_slice().to_vec();
    // The level byte, named rather than counted from the end: the record id
    // is variable width, so an offset measured backwards would land
    // somewhere else the moment a test used a different id.
    bytes[INDEX_PREFIX_LEN.saturating_add(8)] = 1;
    assert!(matches!(
        SpatialIndexKey::decode(&bytes),
        Err(Error::NoSuchCell { .. })
    ));
}

#[test]
fn a_box_survives_the_round_trip_and_a_backwards_one_is_refused() {
    let held =
        SpatialExtent::new(Bounds::of_position(at(-1_000, -2_000)).widened_to(at(3_000, 4_000)));
    let encoded = held.encode();
    assert_eq!(SpatialExtent::decode(encoded.as_slice()).unwrap(), held);

    // West east of east: not a box any geometry produces, and a rectangle
    // that would answer `meets` for a strip while holding no position.
    let mut bytes = encoded.as_slice().to_vec();
    let body = bytes.len().saturating_sub(32);
    bytes[body..body.saturating_add(8)].copy_from_slice(&[0xFF; 8]);
    assert!(matches!(
        SpatialExtent::decode(&bytes),
        Err(Error::NotABox { .. })
    ));
}
