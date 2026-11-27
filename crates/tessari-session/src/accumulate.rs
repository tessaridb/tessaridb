//! What a fold holds while its group is still arriving.
//!
//! # Why an accumulator rather than a collection
//!
//! [`crate::aggregate::grouped`] used to keep, for every fold occurrence, one
//! value per record of the group. So `SELECT city, count(*) FROM t GROUP BY
//! city` over fifty thousand records with three cities held fifty thousand
//! values to answer three records: the intermediate exceeded the answer by the
//! whole table.
//!
//! Most folds this store has — `count`, `sum`, `mean`, `min`, `max`, and the
//! two statistical ones — can be computed one value at a time. So that case
//! wants an accumulator, not a spill: a group holds **one of these per fold
//! occurrence**, whatever the group's size.
//!
//! # The two that cannot, and why they are named rather than tolerated
//!
//! `collect`'s answer **is** the collection and an exact `median` has to see
//! every value before it knows which one is the middle, so neither reduces as it
//! goes. The claim above was once "every fold", asserted by a test offering
//! fifty thousand values; the honest repair is not to weaken that assertion to
//! `held() <= n`, which would delete the guarantee for the seven folds that
//! still have it. Instead [`Aggregate::retention`] names the exceptions as a
//! set, the assertion is made **per class**, and the test additionally checks
//! that a whole-group fold really does hold its group — so the classification is
//! read rather than merely declared, and the executor's memory ceiling reads the
//! same set (Q-227).
//!
//! # What it did not do, measured
//!
//! It did not move a grouping read's **peak**, and the measurement says so:
//! folding fifty thousand records into one answer costs `+45 823 KiB` before and
//! after, one kibibyte above reading the whole table. The fold consumes the
//! records it was handed, so each record is freed as its value is folded and
//! live memory *falls* through the fold — the collection was real and was never
//! at the peak. A grouping read's peak is the source's materialised vector, the
//! same conclusion the bounded sort and the whole-table read reached before it.
//!
//! So this is a **precondition** rather than a win on its own: it removes the
//! one part of a fold that grew with the input, which is what would otherwise
//! become the dominant cost the day the source stops materialising. Claiming a
//! saving here would be claiming the instrument's silence as a number.
//!
//! # Why the batch fold was kept rather than deleted
//!
//! [`crate::aggregate::fold`] is the reference this is tested against. An
//! incremental total that agrees with itself proves nothing; one that agrees
//! with the implementation it replaced, over a corpus of mixed kinds, proves
//! what the change claims.
//!
//! # Where equivalence is not free
//!
//! `sum` chooses between an exact total and a float one **after** seeing every
//! number, and float addition is not associative — so totalling exactly and
//! converting when the first float arrives would differ in the last bits. Both
//! totals are therefore carried and the choice is still made at the end.
//!
//! For the same reason arithmetic failures are **deferred to the end**: the
//! batch fold can only fail in the branch it chose, so a group of integers must
//! not start failing on a float limit it never reaches. A **type** error still
//! raises where it is met, which keeps the batch fold's rule that a value of the
//! wrong kind outranks a number out of range — there, because the type check ran
//! over the whole group before any arithmetic did; here, because the arithmetic
//! failure waits.
//!
//! What does change is *which* error a read with two failing folds reports: the
//! batch fold met them in occurrence order, and this meets them in record order.
//! Both name the same defect in the same statement.

mod arithmetic;
mod counter;
mod distinct;
pub(crate) mod exact;
mod holding;
mod partial;
mod quantile;
mod sequence;
mod sketch;
use rust_decimal::Decimal;
#[cfg(test)]
use tessari_ql::Retention;
use tessari_ql::{Aggregate, COUNT_DISTINCT_MOST, ExprKind, Span};
use tessari_types::{Number, Value};

use crate::aggregate::{approximate, present};
use crate::error::{Error, Result};
pub(crate) use arithmetic::{
    Moments, Moments2, add_exact, add_float, count, exact_ranks, failed, gini, histogram, middle,
    summable,
};
pub(crate) use exact::ExactSum;
pub(crate) use sketch::estimated;

/// A running value that may already have failed, holding why.
///
/// The failure is carried rather than raised so that it can be discarded
/// unread — which is the whole point, since only one of `sum`'s two totals is
/// ever the answer.
type Running<T> = core::result::Result<T, &'static str>;

/// One fold in progress, reduced to what it still needs.
pub(crate) enum Accumulator {
    /// How many of the values offered were values at all.
    Count {
        /// The count so far.
        seen: u64,
    },
    /// A total in both kinds, because which one answers is not yet known.
    Sum {
        /// The exact running total.
        exact: Running<Decimal>,
        /// The same total as floats, held exactly (ADR-0114).
        float: Running<ExactSum>,
        /// Whether any number offered was a float.
        saw_float: bool,
        /// Whether every number offered was an integer.
        all_integer: bool,
        /// Where to point a failure.
        span: Span,
    },
    /// An exact running total and how many numbers went into it.
    Mean {
        /// The exact running total.
        exact: Running<Decimal>,
        /// The same total as floats, held exactly — the answer once a float
        /// arrives, as `sum`'s is (ADR-0114 D3).
        float: Running<ExactSum>,
        /// Whether any number offered was a float.
        saw_float: bool,
        /// How many numbers it holds.
        counted: i64,
        /// Where to point a failure.
        span: Span,
    },
    /// The smallest or largest value offered, in the value system's order.
    Extreme {
        /// Which end is wanted.
        smallest: bool,
        /// The value holding that end so far.
        held: Option<Value>,
    },
    /// A count and two exact totals, and whether to take the root.
    Spread {
        /// Whether the answer is `stddev` rather than `variance`.
        rooted: bool,
        /// The running state, or why it stopped being computable.
        running: Running<Moments>,
        /// Where to point a failure.
        span: Span,
    },
    /// Every number offered, because the middle one is not known until the end.
    Middle {
        /// The numbers so far, unsorted — sorting once at the end costs
        /// `n log n` where keeping the vector sorted costs `n²` moves.
        held: Vec<Value>,
        /// The other parts' numbers as exact values and how many times each
        /// was offered, merged from a leader's state (ADR-0121 D1).
        runs: std::collections::BTreeMap<Decimal, u64>,
        /// Where to point a failure.
        span: Span,
    },
    /// `mad`'s numbers — `median`'s holding, read twice at the finish: once
    /// for the middle, once for the middle of the distances from it.
    Deviation {
        /// The numbers so far, unsorted, as `median` holds them.
        held: Vec<Value>,
        /// How many records held each value already settled.
        runs: std::collections::BTreeMap<Decimal, u64>,
        /// Where to point a refusal.
        span: Span,
    },
    /// The keyed folds' pairs — each present value with the key it is folded
    /// under — held whole and grouped at the finish, where each key's own
    /// accumulator answers exactly what the plain fold would.
    Keyed {
        /// Which of the three the answer is read as.
        fold: Aggregate,
        /// The pairs, in arrival order.
        held: Vec<(String, Value)>,
        /// Where to point a refusal.
        span: Span,
    },
    /// The product-analytics folds' events — the instant and the conditions
    /// each record met — held whole because no running form knows the
    /// longest chain before the last event has arrived.
    Events {
        /// Which of the four the answer is read as.
        fold: Aggregate,
        /// The events, in arrival order; sorted by instant at the finish.
        held: Vec<sequence::Event>,
        /// The window `window_funnel` keeps a chain inside, settled once.
        window: Option<f64>,
        /// The pattern the sequence folds read, settled and parsed once.
        pattern: Option<Vec<sequence::Step>>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `string_agg`'s strings and the separator they join with — `collect`'s
    /// holding, rendered once at the finish.
    Joined {
        /// The strings so far, in arrival order.
        held: Vec<String>,
        /// The separator, once a record has offered it.
        sep: Option<String>,
        /// Where to point a refusal.
        span: Span,
    },
    /// The counter folds: every sample and the instant it was observed at,
    /// ordered by the instant when the group finishes (ADR-0088 §5).
    Counter {
        /// Which of the six.
        fold: Aggregate,
        /// The samples, in record order.
        held: Vec<(tessari_types::Datetime, Number)>,
        /// The parts merged so far in key order, each a leader's summary or
        /// its samples, with this node's own samples between them (ADR-0121 D3).
        parts: Vec<counter::Part>,
        /// The horizon `predict_linear` projects to, in seconds past the last
        /// sample — the last one offered, since the expression is one value
        /// wherever it reads no record. `None` for every other fold.
        ahead: Option<f64>,
        /// Where to point a failure.
        span: Span,
    },
    /// Every present value, in the order the records arrived.
    Every {
        /// What has been offered so far, which is also the answer.
        held: Vec<Value>,
    },
    /// `approx_distinct`'s sketch (ADR-0122 C2).
    Distinct {
        /// The sketch so far.
        sketch: distinct::Distinct,
    },
    /// `count(DISTINCT …)`: every distinct present value, by the store's `=`
    /// (ADR-0134 D8), up to [`tessari_ql::COUNT_DISTINCT_MOST`].
    Unique {
        /// The values so far.
        held: std::collections::BTreeSet<Value>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `top_k`'s counts and the `k` it answers at — exact, so bounded by the
    /// same ceiling `count(DISTINCT …)` holds (ADR-0134 D8).
    Common {
        /// How many records held each value.
        held: std::collections::BTreeMap<Value, u64>,
        /// The `k`, once a record has offered it.
        asked: Option<u64>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `approx_quantile`'s sketch and the rank it answers at (ADR-0122 C3).
    Quantile {
        /// The sketch so far.
        sketch: quantile::Quantile,
        /// The rank, once a record has offered it.
        rank: Option<f64>,
        /// Where to point a failure.
        span: Span,
    },
    /// The paired folds' running sums — `corr`, `covar` and the three
    /// `regr_` folds all answer from one bivariate update, in space that
    /// does not grow with the group.
    Paired {
        /// Which of the five the answer is read as.
        fold: Aggregate,
        /// The sums so far.
        running: Moments2,
        /// Where to point a refusal.
        span: Span,
    },
    /// `quantiles`' sketch and the ranks it answers at — one sketch, several
    /// ranks, each answer under the same ADR-0122 C3 bound `approx_quantile`
    /// carries.
    Quantiles {
        /// The sketch so far.
        sketch: quantile::Quantile,
        /// The ranks, once a record has offered them.
        ranks: Option<Vec<f64>>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `exact_quantiles`' multiset and ranks — `median`'s holding, answered
    /// at ⌊rank × (n − 1)⌋, the definition the sketch estimates.
    ExactQuantiles {
        /// The values so far.
        held: Vec<Value>,
        /// How many records held each value already settled.
        runs: std::collections::BTreeMap<Decimal, u64>,
        /// The ranks, once a record has offered them.
        ranks: Option<Vec<f64>>,
        /// Where to point a refusal.
        span: Span,
    },
    /// The shape folds' central moments — `skewness` and `kurtosis` answer
    /// from one running update, in space that does not grow with the group.
    Shaped {
        /// Which of the two the answer is read as.
        fold: Aggregate,
        /// The moments so far.
        running: arithmetic::Shape,
        /// Where to point a refusal.
        span: Span,
    },
    /// `entropy`'s frequencies — how many records held each distinct value,
    /// bounded like `count(DISTINCT …)` because the answer reads them all.
    Frequencies {
        /// How many records held each value.
        held: std::collections::BTreeMap<Value, u64>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `gini`'s multiset — `median`'s holding, read in order at the finish.
    Gini {
        /// The values so far.
        held: Vec<Value>,
        /// How many records held each value already settled.
        runs: std::collections::BTreeMap<Decimal, u64>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `histogram`'s numbers and the bin count it answers at.
    Binned {
        /// The numbers so far.
        held: Vec<f64>,
        /// The bin count, once a record has offered it.
        asked: Option<u64>,
        /// Where to point a refusal.
        span: Span,
    },
    /// `state::merge`'s one accumulator: the states offered all name one fold,
    /// and this is that fold's accumulator, merged into (G102 S11).
    Merging {
        /// The fold the first state named, and the accumulator merging its
        /// states — `None` until one arrives.
        held: Option<(String, Box<Accumulator>)>,
        /// Where to point a refusal.
        span: Span,
    },
}

impl Accumulator {
    /// The accumulator this fold occurrence needs.
    ///
    /// Takes the expression's kind rather than the aggregate so that the caller
    /// keeps one list of occurrences and does not build a second one alongside
    /// it — two lists indexed by position are two chances for the positions to
    /// stop corresponding. Only a fold ever reaches here, because the only
    /// producer of occurrences yields folds; anything else would still occupy
    /// its position, which is the property that matters.
    pub(crate) fn of(kind: &ExprKind) -> Self {
        let ExprKind::Fold { fold, span, .. } = kind else {
            return Self::Count { seen: 0 };
        };
        Self::for_aggregate(*fold, *span)
    }

    /// The accumulator for one aggregate.
    pub(crate) fn for_aggregate(aggregate: Aggregate, span: Span) -> Self {
        match aggregate {
            Aggregate::Count => Self::Count { seen: 0 },
            Aggregate::Sum => Self::Sum {
                exact: Ok(Decimal::ZERO),
                float: Ok(ExactSum::default()),
                saw_float: false,
                all_integer: true,
                span,
            },
            Aggregate::Mean => Self::Mean {
                exact: Ok(Decimal::ZERO),
                float: Ok(ExactSum::default()),
                saw_float: false,
                counted: 0,
                span,
            },
            Aggregate::Min => Self::Extreme {
                smallest: true,
                held: None,
            },
            Aggregate::Max => Self::Extreme {
                smallest: false,
                held: None,
            },
            Aggregate::Variance => Self::Spread {
                rooted: false,
                running: Ok(Moments::default()),
                span,
            },
            Aggregate::Stddev => Self::Spread {
                rooted: true,
                running: Ok(Moments::default()),
                span,
            },
            Aggregate::Median => Self::Middle {
                held: Vec::new(),
                runs: std::collections::BTreeMap::new(),
                span,
            },
            Aggregate::Collect => Self::Every { held: Vec::new() },
            Aggregate::CountDistinct => Self::Unique {
                held: std::collections::BTreeSet::new(),
                span,
            },
            Aggregate::ApproxDistinct => Self::Distinct {
                sketch: distinct::Distinct::default(),
            },
            Aggregate::TopK => Self::Common {
                held: std::collections::BTreeMap::new(),
                asked: None,
                span,
            },
            Aggregate::ApproxQuantile => Self::Quantile {
                sketch: quantile::Quantile::default(),
                rank: None,
                span,
            },
            Aggregate::StateMerge => Self::Merging { held: None, span },
            Aggregate::Corr
            | Aggregate::Covar
            | Aggregate::RegrSlope
            | Aggregate::RegrIntercept
            | Aggregate::RegrR2 => Self::Paired {
                fold: aggregate,
                running: Moments2::default(),
                span,
            },
            Aggregate::Quantiles => Self::Quantiles {
                sketch: quantile::Quantile::default(),
                ranks: None,
                span,
            },
            Aggregate::ExactQuantiles => Self::ExactQuantiles {
                held: Vec::new(),
                runs: std::collections::BTreeMap::new(),
                ranks: None,
                span,
            },
            Aggregate::Skewness | Aggregate::Kurtosis => Self::Shaped {
                fold: aggregate,
                running: arithmetic::Shape::default(),
                span,
            },
            Aggregate::Entropy => Self::Frequencies {
                held: std::collections::BTreeMap::new(),
                span,
            },
            Aggregate::Gini => Self::Gini {
                held: Vec::new(),
                runs: std::collections::BTreeMap::new(),
                span,
            },
            Aggregate::Histogram => Self::Binned {
                held: Vec::new(),
                asked: None,
                span,
            },
            Aggregate::Increase
            | Aggregate::Rate
            | Aggregate::Delta
            | Aggregate::Resets
            | Aggregate::Deriv
            | Aggregate::PredictLinear
            | Aggregate::Downsample
            | Aggregate::DecayedSum
            | Aggregate::DecayedMean
            | Aggregate::DecayedMax
            | Aggregate::DecayedCount
            | Aggregate::Changes
            | Aggregate::Irate
            | Aggregate::Idelta
            | Aggregate::TsOfFirst
            | Aggregate::TsOfLast
            | Aggregate::TsOfMax
            | Aggregate::TsOfMin
            | Aggregate::MovingSum
            | Aggregate::MovingMean => Self::Counter {
                fold: aggregate,
                held: Vec::new(),
                parts: Vec::new(),
                ahead: None,
                span,
            },
            Aggregate::Mad => Self::Deviation {
                held: Vec::new(),
                runs: std::collections::BTreeMap::new(),
                span,
            },
            Aggregate::StringAgg => Self::Joined {
                held: Vec::new(),
                sep: None,
                span,
            },
            Aggregate::SumBy | Aggregate::MinBy | Aggregate::MaxBy => Self::Keyed {
                fold: aggregate,
                held: Vec::new(),
                span,
            },
            Aggregate::WindowFunnel
            | Aggregate::SequenceMatch
            | Aggregate::SequenceCount
            | Aggregate::Retention => Self::Events {
                fold: aggregate,
                held: Vec::new(),
                window: None,
                pattern: None,
                span,
            },
        }
    }

    /// Offer this accumulator what one record held for its fold.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSummable`] when a numeric fold is offered a value
    /// that is present and is not a number.
    pub(crate) fn offer(&mut self, value: &Value) -> Result<()> {
        match self {
            Self::Count { seen } => {
                if present(value) {
                    *seen = seen.saturating_add(1);
                }
            }
            Self::Sum {
                exact,
                float,
                saw_float,
                all_integer,
                span,
            } => {
                let Some(number) = summable(value, "sum", *span)? else {
                    return Ok(());
                };
                if matches!(number, Number::Float(_)) {
                    *saw_float = true;
                }
                if !matches!(number, Number::Integer(_)) {
                    *all_integer = false;
                }
                exact_unless_float(exact, number, *saw_float);
                add_float(float, number);
            }
            Self::Mean {
                exact,
                float,
                saw_float,
                counted,
                span,
            } => {
                let Some(number) = summable(value, "mean", *span)? else {
                    return Ok(());
                };
                if matches!(number, Number::Float(_)) {
                    *saw_float = true;
                }
                *counted = counted.saturating_add(1);
                exact_unless_float(exact, number, *saw_float);
                add_float(float, number);
            }
            Self::Extreme { smallest, held } => {
                if !present(value) {
                    return Ok(());
                }
                let replaces = held.as_ref().is_none_or(|kept| (value < kept) == *smallest);
                if replaces {
                    *held = Some(value.clone());
                }
            }
            Self::Spread {
                rooted,
                running,
                span,
            } => {
                let fold = if *rooted { "stddev" } else { "variance" };
                let Some(number) = summable(value, fold, *span)? else {
                    return Ok(());
                };
                let Ok(state) = running else {
                    return Ok(());
                };
                let Some(held) = approximate(number) else {
                    *running = Err("a number no float can hold");
                    return Ok(());
                };
                state.offer(held);
            }
            Self::Middle { held, span, .. } => {
                if summable(value, "median", *span)?.is_some() {
                    held.push(value.clone());
                }
            }
            Self::Deviation { held, span, .. } => {
                if summable(value, "mad", *span)?.is_some() {
                    held.push(value.clone());
                }
            }
            // Offered `[value, key]`: an absent value writes no key, by
            // `sum`'s rule; a key that is not a string is refused rather than
            // rendered, because two values that render alike would silently
            // share one.
            Self::Keyed { fold, held, span } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(key)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                // No key, no entry — an absent label writes nothing, by the
                // same rule an absent value does; a PRESENT key of the wrong
                // kind is a mistake in the statement.
                if !present(key) || !present(sample) {
                    return Ok(());
                }
                let Value::String(key) = key else {
                    return Err(failed(fold.spelling(), "a key that is not a string", *span));
                };
                if *fold == Aggregate::SumBy && summable(sample, fold.spelling(), *span)?.is_none()
                {
                    return Ok(());
                }
                held.push((key.clone(), sample.clone()));
            }
            // Offered `[instant, extra, conditions]` — the window for the
            // funnel, the pattern for the sequence folds — or the conditions
            // alone for `retention`. The extra settles once and refuses
            // drift, by `top_k`'s rule; a condition that is no boolean is
            // refused rather than read as one.
            Self::Events {
                fold,
                held,
                window,
                pattern,
                span,
            } => {
                let Some((at, met, extra)) = sequence::event_of(*fold, value, *span)? else {
                    return Ok(());
                };
                match (*fold, extra) {
                    (Aggregate::WindowFunnel, Some(offered)) => {
                        let Some(seconds) = match &offered {
                            Value::Number(number) => crate::aggregate::approximate(number),
                            _ => None,
                        }
                        .filter(|seconds| *seconds > 0.0) else {
                            return Err(failed(
                                fold.spelling(),
                                "a window in seconds, more than nothing",
                                *span,
                            ));
                        };
                        match window {
                            Some(kept) if *kept != seconds => {
                                return Err(failed(
                                    fold.spelling(),
                                    "a window that differs between records",
                                    *span,
                                ));
                            }
                            Some(_) => {}
                            None => *window = Some(seconds),
                        }
                    }
                    (Aggregate::SequenceMatch | Aggregate::SequenceCount, Some(offered)) => {
                        let Value::String(text) = &offered else {
                            return Err(failed(
                                fold.spelling(),
                                "a pattern written as text, like '(1)(<1800)(2)'",
                                *span,
                            ));
                        };
                        match pattern {
                            Some(kept) if kept != &sequence::pattern_of(text, *span)? => {
                                return Err(failed(
                                    fold.spelling(),
                                    "a pattern that differs between records",
                                    *span,
                                ));
                            }
                            Some(_) => {}
                            None => *pattern = Some(sequence::pattern_of(text, *span)?),
                        }
                    }
                    _ => {}
                }
                held.push((at, met));
            }
            // Offered `[value, sep]`: the separator is the fold's second
            // argument, settled once like `top_k`'s k; an absent value is
            // passed over by `collect`'s rule, and a present one that is not
            // a string is refused rather than rendered.
            Self::Joined { held, sep, span } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(offered)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                let Value::String(offered) = offered else {
                    return Err(failed(
                        "string_agg",
                        "a separator that is not a string",
                        *span,
                    ));
                };
                match sep {
                    Some(kept) if kept != offered => {
                        return Err(failed(
                            "string_agg",
                            "a separator that differs between records",
                            *span,
                        ));
                    }
                    Some(_) => {}
                    None => *sep = Some(offered.clone()),
                }
                match sample {
                    Value::String(text) => held.push(text.clone()),
                    held_value if present(held_value) => {
                        return Err(failed("string_agg", held_value.type_name(), *span));
                    }
                    _ => {}
                }
            }
            // Offered `[value, instant]`: a sample without a number is not a
            // sample, and one without an instant has no place in the order.
            Self::Counter {
                fold,
                held,
                ahead,
                span,
                ..
            } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(at)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                let Some(number) = summable(sample, fold.spelling(), *span)? else {
                    return Ok(());
                };
                let Value::Datetime(at) = at else {
                    return Err(crate::accumulate::failed(
                        fold.spelling(),
                        "an instant that is not a datetime",
                        *span,
                    ));
                };
                // `predict_linear` carries its horizon as the third element;
                // the last one offered is the one the answer uses.
                if let Some(Value::Number(horizon)) = pair.get(2)
                    && let Some(seconds) = crate::aggregate::approximate(horizon)
                {
                    *ahead = Some(seconds);
                }
                held.push((*at, number.clone()));
            }
            // The one fold that keeps a value of any kind, because it is not
            // computing anything from them — `collect` is the identity fold.
            Self::Every { held } => {
                if present(value) {
                    held.push(value.clone());
                }
            }
            // Offered `[x, y]`: a pair missing a side is not a pair and is
            // passed over; a side of the wrong kind is refused naming the
            // fold — `increase`'s two rules, one per side.
            Self::Paired {
                fold,
                running,
                span,
            } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(x), Some(y)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                let (Some(x), Some(y)) = (
                    summable(x, fold.spelling(), *span)?,
                    summable(y, fold.spelling(), *span)?,
                ) else {
                    return Ok(());
                };
                let (Some(x), Some(y)) = (approximate(x), approximate(y)) else {
                    return Err(failed(fold.spelling(), "a number no float can hold", *span));
                };
                running.offer(x, y);
            }
            Self::Unique { held, span } => {
                if present(value) && held.insert(value.clone()) && held.len() > COUNT_DISTINCT_MOST
                {
                    return Err(Error::DistinctTooMany {
                        most: COUNT_DISTINCT_MOST,
                        span: *span,
                    });
                }
            }
            Self::Distinct { sketch } => {
                if present(value) {
                    sketch.offer(value);
                }
            }
            // Offered `[value, k]`: the k is the fold's second argument, read
            // like the quantile's rank; a value that is absent is not counted.
            Self::Common { held, asked, span } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(wanted)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                settle_k(asked, wanted, *span)?;
                if present(sample) {
                    let entry = held.entry(sample.clone()).or_insert(0);
                    *entry = entry.saturating_add(1);
                    if held.len() > tessari_ql::COUNT_DISTINCT_MOST {
                        return Err(Error::DistinctTooMany {
                            most: tessari_ql::COUNT_DISTINCT_MOST,
                            span: *span,
                        });
                    }
                }
            }
            // Offered `[value, rank]`: the rank is the fold's second argument.
            Self::Quantile { sketch, rank, span } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(asked)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                sketch::settle_rank(rank, asked, *span)?;
                if let Some(number) = summable(sample, "approx_quantile", *span)? {
                    let held = approximate(number)
                        .filter(|held| held.is_finite())
                        .ok_or_else(|| {
                            failed("approx_quantile", "a number no float can hold", *span)
                        })?;
                    sketch.offer(held);
                }
            }
            // Offered `[value, ranks]`: the ranks are the fold's second
            // argument, an array settled once like the quantile's rank.
            Self::Quantiles {
                sketch,
                ranks,
                span,
            } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(asked)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                settle_ranks(ranks, asked, *span)?;
                if let Some(number) = summable(sample, "quantiles", *span)? {
                    let held = approximate(number)
                        .filter(|held| held.is_finite())
                        .ok_or_else(|| failed("quantiles", "a number no float can hold", *span))?;
                    sketch.offer(held);
                }
            }
            // Offered `[value, ranks]` like `quantiles`, held like `median`.
            Self::ExactQuantiles {
                held,
                runs: _,
                ranks,
                span,
            } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(asked)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                settle_ranks(ranks, asked, *span)?;
                if summable(sample, "exact_quantiles", *span)?.is_some() {
                    held.push(sample.clone());
                }
            }
            Self::Shaped {
                fold,
                running,
                span,
            } => {
                if let Some(number) = summable(value, fold.spelling(), *span)? {
                    let held = approximate(number)
                        .filter(|held| held.is_finite())
                        .ok_or_else(|| {
                            failed(fold.spelling(), "a number no float can hold", *span)
                        })?;
                    running.offer(held);
                }
            }
            Self::Frequencies { held, span } => {
                if present(value) {
                    let entry = held.entry(value.clone()).or_insert(0);
                    *entry = entry.saturating_add(1);
                    if held.len() > tessari_ql::COUNT_DISTINCT_MOST {
                        return Err(Error::DistinctTooMany {
                            most: tessari_ql::COUNT_DISTINCT_MOST,
                            span: *span,
                        });
                    }
                }
            }
            Self::Gini { held, span, .. } => {
                if let Some(number) = summable(value, "gini", *span)? {
                    let below_zero = number
                        .as_decimal()
                        .map(|held| held < Decimal::ZERO)
                        .unwrap_or_else(|| approximate(number).is_some_and(|held| held < 0.0));
                    if below_zero {
                        return Err(failed(
                            "gini",
                            "a value below zero, which equality has no meaning over",
                            *span,
                        ));
                    }
                    held.push(value.clone());
                }
            }
            // Offered `[value, bins]`: the bin count is the fold's second
            // argument, settled once like `top_k`'s k.
            Self::Binned { held, asked, span } => {
                let Value::Array(pair) = value else {
                    return Ok(());
                };
                let (Some(sample), Some(wanted)) = (pair.first(), pair.get(1)) else {
                    return Ok(());
                };
                settle_k(asked, wanted, *span)?;
                if let Some(number) = summable(sample, "histogram", *span)? {
                    let held_number = approximate(number)
                        .filter(|held| held.is_finite())
                        .ok_or_else(|| failed("histogram", "a number no float can hold", *span))?;
                    held.push(held_number);
                }
            }
            // A state of one fold, merged into that fold's accumulator. The
            // first state names the fold; a state of any other refuses rather
            // than mis-adds, which is the whole reason the name travels.
            Self::Merging { held, span } => {
                if !present(value) {
                    return Ok(());
                }
                let Value::FoldState(state) = value else {
                    return Err(crate::error::Error::StateExpected {
                        kind: value.type_name(),
                        span: *span,
                    });
                };
                match held {
                    Some((fold, accumulator)) => {
                        if *fold != state.fold {
                            return Err(crate::error::Error::StateFoldMismatch {
                                merging: fold.clone(),
                                held: state.fold.clone(),
                                span: *span,
                            });
                        }
                        if !accumulator.merge(&state.state)? {
                            return Err(crate::error::Error::StateFoldMismatch {
                                merging: fold.clone(),
                                held: state.fold.clone(),
                                span: *span,
                            });
                        }
                    }
                    slot @ None => {
                        let Some(fold) = tessari_ql::Aggregate::parse(&state.fold) else {
                            return Err(crate::error::Error::StateFoldMismatch {
                                merging: state.fold.clone(),
                                held: state.fold.clone(),
                                span: *span,
                            });
                        };
                        let mut accumulator = Box::new(Accumulator::for_aggregate(fold, *span));
                        if !accumulator.merge(&state.state)? {
                            return Err(crate::error::Error::StateFoldMismatch {
                                merging: state.fold.clone(),
                                held: state.fold.clone(),
                                span: *span,
                            });
                        }
                        *slot = Some((state.fold.clone(), accumulator));
                    }
                }
            }
        }
        Ok(())
    }

    /// What this fold answers for its group.
    ///
    /// Borrows rather than consumes so the caller can walk its accumulators by
    /// position, which is how it matches them to the projection they belong to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSummable`] when the total the answer needs went out
    /// of range. The total the answer does **not** need may have failed and is
    /// discarded unread.
    pub(crate) fn finish(&self) -> Result<Value> {
        match self {
            Self::Count { seen } => Ok(Value::Number(Number::Integer(
                i64::try_from(*seen).unwrap_or(i64::MAX),
            ))),
            Self::Sum {
                exact,
                float,
                saw_float,
                all_integer,
                span,
            } => {
                if *saw_float {
                    let total = float
                        .as_ref()
                        .map_err(|reason| failed("sum", reason, *span))?
                        .total()
                        .map_err(|reason| failed("sum", reason, *span))?;
                    return Ok(Value::Number(Number::float(total)));
                }
                let total = exact.map_err(|reason| failed("sum", reason, *span))?;
                // Over nothing this is zero, deliberately: a sum that answered
                // `NONE` for an empty group would make every caller write the
                // same `?? 0`.
                if *all_integer && let Ok(whole) = i64::try_from(total) {
                    return Ok(Value::Number(Number::Integer(whole)));
                }
                Ok(Value::Number(Number::Decimal(total)))
            }
            Self::Mean {
                exact,
                float,
                saw_float,
                counted,
                span,
            } => {
                if *counted == 0 {
                    // An average of no numbers is not a number, and zero would
                    // be a claim.
                    return Ok(Value::None);
                }
                if *saw_float {
                    let total = float
                        .as_ref()
                        .map_err(|reason| failed("mean", reason, *span))?
                        .total()
                        .map_err(|reason| failed("mean", reason, *span))?;
                    let counted = arithmetic::count(u64::try_from(*counted).unwrap_or(0));
                    return Ok(Value::Number(Number::float(total / counted)));
                }
                let total = exact.map_err(|reason| failed("mean", reason, *span))?;
                let averaged = total
                    .checked_div(Decimal::from(*counted))
                    .ok_or_else(|| failed("mean", "a group of no size", *span))?;
                Ok(Value::Number(Number::Decimal(averaged)))
            }
            Self::Extreme { held, .. } => Ok(held.clone().unwrap_or(Value::None)),
            Self::Spread {
                rooted,
                running,
                span,
            } => {
                let fold = if *rooted { "stddev" } else { "variance" };
                let state = running
                    .as_ref()
                    .map_err(|reason| failed(fold, reason, *span))?;
                let Some(spread) = state
                    .variance()
                    .map_err(|reason| failed(fold, reason, *span))?
                else {
                    return Ok(Value::None);
                };
                let answer = if *rooted { spread.sqrt() } else { spread };
                Ok(Value::Number(Number::float(answer)))
            }
            Self::Middle { held, runs, span } => middle(held, runs, *span),
            Self::Deviation { held, runs, span } => arithmetic::deviation(held, runs, *span),
            // Over nothing this is the empty array, deliberately, and for the
            // reason `sum` answers zero: an answer every caller has to write
            // `?? []` after is the wrong answer.
            Self::Every { held } => Ok(Value::Array(held.clone())),
            // Over nothing this is the empty string, by `collect`'s rule: the
            // empty join is the join's identity, and an answer every caller
            // has to write `?? ''` after is the wrong answer.
            Self::Joined { held, sep, .. } => {
                Ok(Value::String(held.join(sep.as_deref().unwrap_or_default())))
            }
            Self::Keyed { fold, held, span } => {
                let inner = match fold {
                    Aggregate::SumBy => Aggregate::Sum,
                    Aggregate::MinBy => Aggregate::Min,
                    _ => Aggregate::Max,
                };
                let mut by_key: std::collections::BTreeMap<String, Accumulator> =
                    std::collections::BTreeMap::new();
                for (key, value) in held {
                    let entry = by_key
                        .entry(key.clone())
                        .or_insert_with(|| Accumulator::for_aggregate(inner, *span));
                    entry.offer(value)?;
                }
                if by_key.len() > COUNT_DISTINCT_MOST {
                    return Err(Error::DistinctTooMany {
                        most: COUNT_DISTINCT_MOST,
                        span: *span,
                    });
                }
                let mut answered = std::collections::BTreeMap::new();
                for (key, entry) in &by_key {
                    answered.insert(key.clone(), entry.finish()?);
                }
                Ok(Value::Object(answered))
            }
            Self::Events {
                fold,
                held,
                window,
                pattern,
                span,
            } => match fold {
                Aggregate::Retention => Ok(sequence::retained(held)),
                Aggregate::WindowFunnel => {
                    let Some(window) = window else {
                        return Err(failed(
                            fold.spelling(),
                            "a window in seconds, more than nothing",
                            *span,
                        ));
                    };
                    Ok(sequence::funnel(held, *window))
                }
                Aggregate::SequenceMatch | Aggregate::SequenceCount => {
                    let Some(pattern) = pattern else {
                        return Err(failed(
                            fold.spelling(),
                            "a pattern written as text, like '(1)(<1800)(2)'",
                            *span,
                        ));
                    };
                    // A step naming a condition the statement never wrote is
                    // refused rather than never met — the two read the same in
                    // the answer and only one of them is true.
                    let width = held.first().map_or(0, |(_, met)| met.len());
                    if pattern.iter().any(|step| step.cond >= width) {
                        return Err(failed(
                            fold.spelling(),
                            "a pattern naming a condition the call does not write",
                            *span,
                        ));
                    }
                    if matches!(fold, Aggregate::SequenceMatch) {
                        Ok(sequence::matched(held, pattern))
                    } else {
                        Ok(sequence::counted(held, pattern))
                    }
                }
                _ => unreachable!("only a sequence fold holds events"),
            },
            Self::Unique { held, .. } => Ok(Value::Number(Number::Integer(
                i64::try_from(held.len()).unwrap_or(i64::MAX),
            ))),
            Self::Counter {
                fold,
                held,
                parts,
                ahead,
                span,
            } => counter::finish(*fold, held, parts, *ahead, *span),
            Self::Distinct { sketch } => Ok(sketch.answer()),
            Self::Paired {
                fold,
                running,
                span,
            } => {
                let answered = match fold {
                    Aggregate::Corr => running.correlation(),
                    Aggregate::Covar => running.covariance(),
                    Aggregate::RegrSlope => running.slope(),
                    Aggregate::RegrIntercept => running.intercept(),
                    Aggregate::RegrR2 => running.r2(),
                    _ => unreachable!("only a paired fold holds a Moments2"),
                }
                .map_err(|reason| failed(fold.spelling(), reason, *span))?;
                Ok(answered.map_or(Value::None, |held| Value::Number(Number::float(held))))
            }
            Self::Quantiles { sketch, ranks, .. } => {
                Ok(ranks.as_ref().map_or(Value::None, |ranks| {
                    Value::Array(ranks.iter().map(|rank| sketch.answer(*rank)).collect())
                }))
            }
            Self::ExactQuantiles {
                held,
                runs,
                ranks,
                span,
            } => match ranks {
                None => Ok(Value::None),
                Some(ranks) => exact_ranks(held, runs, ranks, *span),
            },
            Self::Shaped { fold, running, .. } => {
                let answered = match fold {
                    Aggregate::Skewness => running.skewness(),
                    Aggregate::Kurtosis => running.kurtosis(),
                    _ => unreachable!("only a shape fold holds a Shape"),
                };
                Ok(answered.map_or(Value::None, |held| Value::Number(Number::float(held))))
            }
            Self::Frequencies { held, .. } => {
                let total: u64 = held
                    .values()
                    .fold(0, |total, many| total.saturating_add(*many));
                if total == 0 {
                    return Ok(Value::None);
                }
                let total = count(total);
                let bits: f64 = held
                    .values()
                    .map(|many| {
                        let share = count(*many) / total;
                        -share * share.log2()
                    })
                    .sum();
                Ok(Value::Number(Number::float(bits)))
            }
            Self::Gini { held, runs, span } => gini(held, runs, *span),
            Self::Binned { held, asked, span } => {
                let Some(asked) = *asked else {
                    return Err(failed(
                        "histogram",
                        "a whole number of bins to answer",
                        *span,
                    ));
                };
                if asked == 0 {
                    return Err(failed("histogram", "at least one bin", *span));
                }
                Ok(histogram(held, asked))
            }
            Self::Quantile { sketch, rank, .. } => {
                Ok(rank.map_or(Value::None, |rank| sketch.answer(rank)))
            }
            Self::Common { held, asked, span } => {
                let Some(asked) = *asked else {
                    return Err(failed("top_k", "a whole number of values to answer", *span));
                };
                let mut ranked: Vec<(&Value, u64)> =
                    held.iter().map(|(value, many)| (value, *many)).collect();
                ranked.sort_by(|(one_value, one), (other_value, other)| {
                    other.cmp(one).then_with(|| one_value.cmp(other_value))
                });
                let answered: Vec<Value> = ranked
                    .into_iter()
                    .take(usize::try_from(asked).unwrap_or(usize::MAX))
                    .map(|(value, many)| {
                        let mut held = std::collections::BTreeMap::new();
                        held.insert(
                            "count".to_string(),
                            Value::Number(Number::Integer(i64::try_from(many).unwrap_or(i64::MAX))),
                        );
                        held.insert("value".to_string(), value.clone());
                        Value::Object(held)
                    })
                    .collect();
                Ok(Value::Array(answered))
            }
            // The merged state, named by its fold — finished by
            // `state::finish`, or stored and merged again. A merge nothing was
            // offered answers `NONE`, the absence every fold answers over
            // nothing but `count` and `sum`.
            Self::Merging { held, span } => match held {
                None => Ok(Value::None),
                Some((fold, accumulator)) => {
                    let state =
                        accumulator
                            .state()
                            .ok_or(crate::error::Error::StateCannotTravel {
                                fold: tessari_ql::Aggregate::parse(fold)
                                    .map_or("fold", tessari_ql::Aggregate::spelling),
                                span: *span,
                            })?;
                    Ok(Value::FoldState(Box::new(tessari_types::FoldState {
                        fold: fold.clone(),
                        state,
                    })))
                }
            },
        }
    }

    /// How many values this accumulator is still holding.
    ///
    /// The counter behind the claim: a constant-space fold holds at most one
    /// value however many it is offered, so what such a group costs is set by
    /// its folds and not by its records.
    ///
    /// **Exhaustive on purpose.** This used to end in `_ => 0`, which reported
    /// zero for anything unlisted — and the one thing that catch-all could ever
    /// hide is a fold that retains, which is precisely what this counter exists
    /// to measure. A new variant should break this match and make somebody say
    /// what it costs.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        match self {
            Self::Count { .. }
            | Self::Sum { .. }
            | Self::Mean { .. }
            | Self::Spread { .. }
            | Self::Paired { .. }
            // A sketch holds no value it was offered; what it keeps is
            // bounded by its own size (ADR-0122 C2, C3), not by the group's.
            | Self::Distinct { .. }
            | Self::Quantile { .. }
            | Self::Quantiles { .. }
            | Self::Shaped { .. } => 0,
            Self::Extreme { held, .. } => usize::from(held.is_some()),
            Self::Middle { held, runs, .. }
            | Self::Deviation { held, runs, .. }
            | Self::ExactQuantiles { held, runs, .. }
            | Self::Gini { held, runs, .. } => held.len().saturating_add(runs.len()),
            Self::Frequencies { held, .. } => held.len(),
            Self::Binned { held, .. } => held.len(),
            Self::Every { held } => held.len(),
            Self::Keyed { held, .. } => held.len(),
            Self::Events { held, .. } => held.len(),
            Self::Joined { held, .. } => held.len(),
            Self::Unique { held, .. } => held.len(),
            Self::Common { held, .. } => held.len(),
            Self::Counter { held, parts, .. } => held.len().saturating_add(parts.len()),
            // One accumulator however many states merged into it — constant
            // by the inner fold's own retention.
            Self::Merging { .. } => 0,
        }
    }
}

/// Add to the exact total while it can still be the answer.
///
/// Once a float has arrived the float total answers, whatever follows, so the
/// exact one is dropped rather than carried: converting each float to a decimal
/// was most of what a `sum` over floats cost (ADR-0114, measured).
/// The `k` a `top_k` answers at, read like the quantile's rank: a whole
/// number, and the same one on every record — a `k` that differs between
/// records is a question with two answers, refused rather than picked from.
fn settle_k(held: &mut Option<u64>, asked: &Value, span: Span) -> Result<()> {
    let Value::Number(number) = asked else {
        return Err(failed("top_k", "a count that is not a number", span));
    };
    let Some(wanted) = number.as_exact_integer().filter(|wanted| *wanted >= 0) else {
        return Err(failed("top_k", "a whole count of none or more", span));
    };
    let wanted =
        u64::try_from(wanted).map_err(|_| failed("top_k", "a count past what fits", span))?;
    match held {
        Some(kept) if *kept != wanted => Err(failed(
            "top_k",
            "a count that differs between records",
            span,
        )),
        Some(_) => Ok(()),
        None => {
            *held = Some(wanted);
            Ok(())
        }
    }
}

/// The ranks a `quantiles` fold answers at, read like `top_k`'s k: an array
/// of numbers, each from 0 to 1, settled once and refused when a later
/// record offers a different set.
fn settle_ranks(held: &mut Option<Vec<f64>>, asked: &Value, span: Span) -> Result<()> {
    let Value::Array(asked) = asked else {
        return Err(failed("quantiles", "ranks that are not an array", span));
    };
    let mut wanted = Vec::with_capacity(asked.len());
    for rank in asked {
        let Value::Number(rank) = rank else {
            return Err(failed("quantiles", "a rank that is not a number", span));
        };
        let Some(rank) = approximate(rank).filter(|rank| (0.0..=1.0).contains(rank)) else {
            return Err(failed("quantiles", "a rank outside 0 to 1", span));
        };
        wanted.push(rank);
    }
    match held {
        Some(kept) if *kept != wanted => Err(failed(
            "quantiles",
            "ranks that differ between records",
            span,
        )),
        Some(_) => Ok(()),
        None => {
            *held = Some(wanted);
            Ok(())
        }
    }
}

fn exact_unless_float(exact: &mut Running<Decimal>, number: &Number, saw_float: bool) {
    if saw_float {
        *exact = Err("a total a float answers instead");
    } else {
        add_exact(exact, number);
    }
}

/// One exact number as a value, with trailing zeroes gone.
///
/// Normalised because the scale is the last place the answer could still
/// remember which of several equal numbers it came from.
fn exactly(held: &Decimal) -> Value {
    Value::Number(Number::Decimal(held.normalize()))
}

/// One already-checked number as an exact decimal.
fn exact(value: &Value, span: Span) -> Result<Decimal> {
    let Value::Number(number) = value else {
        return Err(failed("median", value.type_name(), span));
    };
    number
        .as_decimal()
        .ok_or_else(|| failed("median", "a number outside the exact range", span))
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a unit test that cannot fail loudly is the failure this module's own history is about"
)]
mod tests;
