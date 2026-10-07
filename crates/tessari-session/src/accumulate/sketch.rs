//! The approximate folds' rank, travelling state and merge (ADR-0122 C4, C5).
//!
//! `approx_distinct` travels as its sketch's bytes. `approx_quantile` travels
//! as `[rank, bytes]`: the sketch does not depend on the rank, but a node that
//! held no record of a group learns the rank only from the part that did.
//! Two parts asked at two ranks do not merge, and the read gathers records
//! instead, which then refuses the statement in its own words.

use tessari_ql::{Aggregate, Span};
use tessari_types::{Number, Value};

use super::distinct::{self, Distinct};
use super::quantile::{self, Quantile};
use super::{Accumulator, failed};
use crate::aggregate::approximate;
use crate::error::Result;
use crate::outcome::Note;

/// The notes an answer holding these folds carries: one per approximate fold,
/// a quantile's saying whether any of `held` collapsed (ADR-0122 C4).
pub(crate) fn estimated<'a>(
    folds: impl Iterator<Item = Aggregate>,
    held: impl Iterator<Item = &'a Accumulator>,
) -> Vec<Note> {
    let folds: Vec<Aggregate> = folds.collect();
    let mut notes = Vec::new();
    if folds.contains(&Aggregate::ApproxDistinct) {
        notes.push(Note::Estimated {
            fold: Aggregate::ApproxDistinct.spelling(),
            method: distinct::METHOD,
            bound: distinct::BOUND,
            collapsed: None,
        });
    }
    if folds.contains(&Aggregate::ApproxQuantile) {
        let collapsed = held.filter_map(Accumulator::collapsed).any(|folded| folded);
        notes.push(Note::Estimated {
            fold: Aggregate::ApproxQuantile.spelling(),
            method: quantile::METHOD,
            bound: quantile::BOUND,
            collapsed: Some(collapsed),
        });
    }
    notes
}

/// Take the rank a record offered, which must be one number from 0 to 1 and
/// the same for every record of the read.
///
/// # Errors
///
/// [`crate::Error::NotSummable`] naming what was wrong with the rank.
pub(super) fn settle_rank(held: &mut Option<f64>, asked: &Value, span: Span) -> Result<()> {
    let Value::Number(number) = asked else {
        return Err(failed(
            "approx_quantile",
            "a rank that is not a number",
            span,
        ));
    };
    let Some(rank) = approximate(number).filter(|rank| (0.0..=1.0).contains(rank)) else {
        return Err(failed("approx_quantile", "a rank outside 0 to 1", span));
    };
    match held {
        Some(kept) if kept.to_bits() != rank.to_bits() => Err(failed(
            "approx_quantile",
            "a rank that differs between records",
            span,
        )),
        Some(_) => Ok(()),
        None => {
            *held = Some(rank);
            Ok(())
        }
    }
}

impl Accumulator {
    /// An approximate fold's state; `None` for any other fold.
    pub(super) fn sketch_state(&self) -> Option<Value> {
        match self {
            Self::Distinct { sketch } => Some(sketch.state()),
            Self::Quantile { sketch, rank, .. } => Some(Value::Array(vec![
                rank.map_or(Value::None, |rank| Value::Number(Number::float(rank))),
                sketch.state(),
            ])),
            _ => None,
        }
    }

    /// Merge an approximate fold's state; `false` for a state of another shape
    /// or a quantile asked at another rank.
    pub(super) fn merge_sketch(&mut self, state: &Value) -> bool {
        match self {
            Self::Distinct { sketch } => Distinct::from_state(state).is_some_and(|more| {
                sketch.absorb(&more);
                true
            }),
            Self::Quantile { sketch, rank, .. } => {
                let Value::Array(held) = state else {
                    return false;
                };
                let [asked, bytes] = held.as_slice() else {
                    return false;
                };
                let asked = match asked {
                    Value::Number(Number::Float(asked)) => Some(*asked),
                    Value::None => None,
                    _ => return false,
                };
                if let (Some(kept), Some(asked)) = (*rank, asked)
                    && kept.to_bits() != asked.to_bits()
                {
                    return false;
                }
                let Some(more) = Quantile::from_state(bytes) else {
                    return false;
                };
                *rank = rank.or(asked);
                sketch.absorb(&more);
                true
            }
            _ => false,
        }
    }

    /// Whether this quantile's sketch folded buckets to keep its bound;
    /// `None` for any other fold.
    pub(crate) fn collapsed(&self) -> Option<bool> {
        match self {
            Self::Quantile { sketch, .. } => Some(sketch.collapsed()),
            _ => None,
        }
    }
}
