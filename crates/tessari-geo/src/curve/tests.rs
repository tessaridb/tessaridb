use std::collections::HashSet;

use super::*;

mod cells;
mod covering;

fn at(longitude: i64, latitude: i64) -> Snapped {
    Snapped::from_units(longitude, latitude).expect("built from the grid's own limits")
}

/// A deterministic sequence. These tests need spread rather than randomness,
/// and a seeded generator keeps a failure reproducible from its own output.
struct Spread(u64);

impl Spread {
    const fn from(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state.wrapping_shl(13);
        state ^= state.wrapping_shr(7);
        state ^= state.wrapping_shl(17);
        self.0 = state;
        state
    }

    /// A value in `[0, bound]`, both ends reachable.
    fn upto(&mut self, bound: u64) -> u64 {
        self.next()
            .checked_rem(bound.saturating_add(1))
            .unwrap_or(0)
    }

    fn longitude(&mut self) -> i64 {
        i64::try_from(self.upto(360_000_000_000))
            .unwrap_or(0)
            .saturating_sub(180_000_000_000)
    }

    fn latitude(&mut self) -> i64 {
        i64::try_from(self.upto(180_000_000_000))
            .unwrap_or(0)
            .saturating_sub(90_000_000_000)
    }

    fn position(&mut self) -> Snapped {
        at(self.longitude(), self.latitude())
    }

    /// A box whose size is drawn from each scale that behaves differently:
    /// a single position, a box narrower than one cell, one a few cells
    /// wide, and one spanning continents.
    ///
    /// The mix is not decoration, and it is not a guess about the workload.
    /// A generator producing only world-scale boxes never lands a box edge
    /// on a cell boundary — the alignment is what a coarse cell cannot
    /// reach — so the two tests written to catch an edge-alignment mistake
    /// both passed over a real one until this method existed. Small boxes
    /// are also what a store is actually asked, which is the lesser reason.
    fn box_of(&mut self) -> Bounds {
        let reach = match self.upto(3) {
            0 => 0,
            1 => 40,
            2 => 5_000,
            _ => 50_000_000_000,
        };
        let anchor = self.position();
        let corner = at(
            anchor
                .longitude_units()
                .saturating_add(i64::try_from(self.upto(reach)).unwrap_or(0))
                .clamp(-180_000_000_000, 180_000_000_000),
            anchor
                .latitude_units()
                .saturating_add(i64::try_from(self.upto(reach)).unwrap_or(0))
                .clamp(-90_000_000_000, 90_000_000_000),
        );
        Bounds::of_position(anchor).widened_to(corner)
    }

    /// A position inside the box, both edges reachable.
    fn inside(&mut self, bounds: Bounds) -> Snapped {
        let width = bounds.east().abs_diff(bounds.west());
        let height = bounds.north().abs_diff(bounds.south());
        at(
            bounds
                .west()
                .saturating_add(i64::try_from(self.upto(width)).unwrap_or(0)),
            bounds
                .south()
                .saturating_add(i64::try_from(self.upto(height)).unwrap_or(0)),
        )
    }
}

/// Whether the covering holds this position's finest cell.
fn covering_holds(cover: &[(Cell, Class)], position: Snapped) -> bool {
    let number = Cell::containing(position).index();
    cover.iter().any(|(cell, _)| {
        let (first, last) = cell.range();
        number >= first && number <= last
    })
}

fn corners(bounds: Bounds) -> [Snapped; 4] {
    [
        at(bounds.west(), bounds.south()),
        at(bounds.west(), bounds.north()),
        at(bounds.east(), bounds.south()),
        at(bounds.east(), bounds.north()),
    ]
}

// C1 — the two directions of the curve.
