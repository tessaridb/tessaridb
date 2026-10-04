use super::*;

/// The smallest offset whose placement reaches this coordinate.
///
/// The inverse of [`place`]'s floor: `place(o) >= c` exactly when
/// `o >= ceil(c * span / LAST)`, so that ceiling is the first offset of the
/// coordinate's own run.
pub(super) fn first_offset(coordinate: u64, span: i128) -> i128 {
    let divisor = i128::from(LAST);
    let numerator = i128::from(coordinate).saturating_mul(span);
    let rounded = numerator
        .saturating_add(divisor.saturating_sub(1))
        .checked_div(divisor)
        .unwrap_or(0);
    rounded.clamp(0, span)
}

/// The curve's number for a cell, at the finest level.
///
/// The standard iterative construction: walk the levels from coarse to fine, read
/// off which quadrant the point is in at each one, then rotate the frame so the
/// next level's quadrants are numbered relative to the direction the curve is
/// travelling. The rotation is the entire difference from a bit interleave, and
/// it is what removes the jumps.
#[must_use]
pub fn hilbert_index(x: u64, y: u64) -> u64 {
    let mut x = x.min(LAST);
    let mut y = y.min(LAST);
    let mut number = 0_u64;
    let mut step = SIDE.wrapping_shr(1);
    while step > 0 {
        let rx = u64::from((x & step) > 0);
        let ry = u64::from((y & step) > 0);
        let quadrant = 3_u64.saturating_mul(rx) ^ ry;
        number = number.saturating_add(step.saturating_mul(step).saturating_mul(quadrant));
        rotate(step, &mut x, &mut y, rx, ry);
        step = step.wrapping_shr(1);
    }
    number
}

/// The cell a curve number names, at the finest level.
///
/// The inverse of [`hilbert_index`]: the same construction run backwards, fine to
/// coarse, undoing each rotation as it goes.
#[must_use]
pub fn hilbert_point(number: u64) -> (u64, u64) {
    let mut remaining = number;
    let mut x = 0_u64;
    let mut y = 0_u64;
    let mut step = 1_u64;
    while step < SIDE {
        let rx = 1 & remaining.wrapping_shr(1);
        let ry = 1 & (remaining ^ rx);
        rotate(step, &mut x, &mut y, rx, ry);
        x = x.saturating_add(step.saturating_mul(rx));
        y = y.saturating_add(step.saturating_mul(ry));
        remaining = remaining.wrapping_shr(2);
        step = step.wrapping_shl(1);
    }
    (x, y)
}

/// Reflect the frame so the next level is read in the curve's own direction.
///
/// Only the two lower quadrants need it, and one of those also needs the axes
/// swapped. Getting this wrong does not produce a broken curve — it produces a
/// *different* traversal that is still a bijection, which is why the inverse test
/// cannot catch it and the adjacency test is the one that does.
pub(super) fn rotate(step: u64, x: &mut u64, y: &mut u64, rx: u64, ry: u64) {
    if ry != 0 {
        return;
    }
    if rx == 1 {
        let edge = step.saturating_sub(1);
        *x = edge.saturating_sub(*x & edge);
        *y = edge.saturating_sub(*y & edge);
    }
    core::mem::swap(x, y);
}
