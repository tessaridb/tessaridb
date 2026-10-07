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
pub(crate) mod exact;
mod holding;
mod partial;
use rust_decimal::Decimal;
#[cfg(test)]
use tessari_ql::Retention;
use tessari_ql::{Aggregate, ExprKind, Span};
use tessari_types::{Number, Value};

use crate::aggregate::{approximate, present};
use crate::error::Result;
pub(crate) use arithmetic::{Moments, add_exact, add_float, failed, middle, summable};
pub(crate) use exact::ExactSum;

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
    /// The counter folds: every sample and the instant it was observed at,
    /// ordered by the instant when the group finishes (ADR-0088 §5).
    Counter {
        /// Which of the three.
        fold: Aggregate,
        /// The samples, in record order.
        held: Vec<(tessari_types::Datetime, Number)>,
        /// The parts merged so far in key order, each a leader's summary or
        /// its samples, with this node's own samples between them (ADR-0121 D3).
        parts: Vec<counter::Part>,
        /// Where to point a failure.
        span: Span,
    },
    /// Every present value, in the order the records arrived.
    Every {
        /// What has been offered so far, which is also the answer.
        held: Vec<Value>,
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
            Aggregate::Increase | Aggregate::Rate | Aggregate::Delta => Self::Counter {
                fold: aggregate,
                held: Vec::new(),
                parts: Vec::new(),
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
            // Offered `[value, instant]`: a sample without a number is not a
            // sample, and one without an instant has no place in the order.
            Self::Counter {
                fold, held, span, ..
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
                held.push((*at, number.clone()));
            }
            // The one fold that keeps a value of any kind, because it is not
            // computing anything from them — `collect` is the identity fold.
            Self::Every { held } => {
                if present(value) {
                    held.push(value.clone());
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
            // Over nothing this is the empty array, deliberately, and for the
            // reason `sum` answers zero: an answer every caller has to write
            // `?? []` after is the wrong answer.
            Self::Every { held } => Ok(Value::Array(held.clone())),
            Self::Counter {
                fold,
                held,
                parts,
                span,
            } => counter::finish(*fold, held, parts, *span),
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
            Self::Count { .. } | Self::Sum { .. } | Self::Mean { .. } | Self::Spread { .. } => 0,
            Self::Extreme { held, .. } => usize::from(held.is_some()),
            Self::Middle { held, runs, .. } => held.len().saturating_add(runs.len()),
            Self::Every { held } => held.len(),
            Self::Counter { held, parts, .. } => held.len().saturating_add(parts.len()),
        }
    }
}

/// Add to the exact total while it can still be the answer.
///
/// Once a float has arrived the float total answers, whatever follows, so the
/// exact one is dropped rather than carried: converting each float to a decimal
/// was most of what a `sum` over floats cost (ADR-0114, measured).
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
