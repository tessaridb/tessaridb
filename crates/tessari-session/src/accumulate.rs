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

use rust_decimal::Decimal;
#[cfg(test)]
use tessari_ql::Retention;
use tessari_ql::{Aggregate, ExprKind, Span};
use tessari_types::{Number, Value};

use crate::aggregate::{approximate, present};
use crate::error::{Error, Result};

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
        /// The same total as a float, summed in the order the values arrived.
        float: Running<f64>,
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
    /// Welford's running `(count, mean, M2)`, and whether to take its root.
    Spread {
        /// Whether the answer is `stddev` rather than `variance`.
        rooted: bool,
        /// The running state, or why it stopped being computable.
        running: Running<Welford>,
        /// Where to point a failure.
        span: Span,
    },
    /// Every number offered, because the middle one is not known until the end.
    Middle {
        /// The numbers so far, unsorted — sorting once at the end costs
        /// `n log n` where keeping the vector sorted costs `n²` moves.
        held: Vec<Value>,
        /// Where to point a failure.
        span: Span,
    },
    /// Every present value, in the order the records arrived.
    Every {
        /// What has been offered so far, which is also the answer.
        held: Vec<Value>,
    },
}

/// Welford's running state: a count, a mean, and the sum of squared deviations.
///
/// Three numbers however long the group is, one pass, and numerically stable.
/// The textbook `E[x²] − E[x]²` form is one subtraction of two large nearly
/// equal numbers and loses every significant digit of the answer on data whose
/// spread is small relative to its magnitude — timestamps and prices, which is
/// most of what anybody takes a variance of.
///
/// Carried in `f64` and not in the exact decimal the other numeric folds use,
/// because `stddev` takes a square root and no square root is exact. A
/// `variance` that promised exactness while the `stddev` beside it could not
/// would be two answers with two different promises out of one pair of folds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Welford {
    /// How many numbers went in.
    counted: u64,
    /// Their running mean.
    mean: f64,
    /// The running sum of squared deviations from that mean.
    m2: f64,
}

impl Welford {
    /// The state before any number has arrived.
    const fn new() -> Self {
        Self {
            counted: 0,
            mean: 0.0,
            m2: 0.0,
        }
    }

    /// Fold one more number in.
    fn offer(&mut self, held: f64) {
        self.counted = self.counted.saturating_add(1);
        // The one lossy conversion here, and it is the divisor of the running
        // mean. Beyond 2^53 a count no longer increments exactly in `f64` — but
        // by then the sum of squared deviations it divides has lost far more,
        // so refusing the cast would buy nothing and there is no wider float.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a count past 2^53 has already made every other number here meaningless"
        )]
        let counted = self.counted as f64;
        let first = held - self.mean;
        self.mean += first / counted;
        let second = held - self.mean;
        self.m2 += first * second;
    }

    /// The sample variance, or `None` when fewer than two numbers arrived.
    ///
    /// `NONE` rather than zero over one value, by the same rule `mean` follows
    /// over none: the spread of a single observation is not zero, it is a
    /// question nobody has enough data to answer, and zero would be a claim.
    fn variance(self) -> Option<f64> {
        if self.counted < 2 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "as above — the divisor is a count, and it is at least one here"
        )]
        let degrees = self.counted.saturating_sub(1) as f64;
        Some(self.m2 / degrees)
    }
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
                float: Ok(0.0_f64),
                saw_float: false,
                all_integer: true,
                span,
            },
            Aggregate::Mean => Self::Mean {
                exact: Ok(Decimal::ZERO),
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
                running: Ok(Welford::new()),
                span,
            },
            Aggregate::Stddev => Self::Spread {
                rooted: true,
                running: Ok(Welford::new()),
                span,
            },
            Aggregate::Median => Self::Middle {
                held: Vec::new(),
                span,
            },
            Aggregate::Collect => Self::Every { held: Vec::new() },
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
                add_exact(exact, number);
                add_float(float, number);
            }
            Self::Mean {
                exact,
                counted,
                span,
            } => {
                let Some(number) = summable(value, "mean", *span)? else {
                    return Ok(());
                };
                *counted = counted.saturating_add(1);
                add_exact(exact, number);
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
            Self::Middle { held, span } => {
                if summable(value, "median", *span)?.is_some() {
                    held.push(value.clone());
                }
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
                    let total = float.map_err(|reason| failed("sum", reason, *span))?;
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
                counted,
                span,
            } => {
                if *counted == 0 {
                    // An average of no numbers is not a number, and zero would
                    // be a claim.
                    return Ok(Value::None);
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
                let state = running.map_err(|reason| failed(fold, reason, *span))?;
                let Some(spread) = state.variance() else {
                    return Ok(Value::None);
                };
                let answer = if *rooted { spread.sqrt() } else { spread };
                Ok(Value::Number(Number::float(answer)))
            }
            Self::Middle { held, span } => middle(held, *span),
            // Over nothing this is the empty array, deliberately, and for the
            // reason `sum` answers zero: an answer every caller has to write
            // `?? []` after is the wrong answer.
            Self::Every { held } => Ok(Value::Array(held.clone())),
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
            Self::Middle { held, .. } | Self::Every { held } => held.len(),
        }
    }
}

/// The middle of these numbers, or `NONE` when there are none.
///
/// An even count answers the mean of the two middles, which is a value that was
/// never in the data. That is tolerable only because the fold is numeric, and it
/// is why `median` refuses the kinds `min` and `max` accept: the same rule over
/// a `datetime` or a `uuid` column would have to construct a value of a kind
/// that has no arithmetic.
///
/// # Why the answer is exact and normalised rather than the value as written
///
/// Answering with the middle value untouched — a column of integers having an
/// integer median, the way `min` and `max` keep what they were given — is the
/// obvious design and it is **not a function of the data**. This store's `Value`
/// deliberately orders and equates numbers across kinds, so `3`, `3.0` and the
/// decimal `3.0` are three equal values that are three different answers on the
/// wire; asked for the middle of those three, "the value as written" is decided
/// entirely by where a sort happened to leave them. The corpus has that row on
/// purpose and it is what caught this.
///
/// So `median` answers exactly, like `mean`, and normalises the result, so that
/// the same multiset of numbers gives the same answer whatever order the records
/// arrive in and whichever kinds they were written as. It costs the kind and buys
/// a determinism the alternative cannot state — and it is the same promise the
/// other numeric fold already makes, including the same refusal of a number no
/// exact form holds.
fn middle(held: &[Value], span: Span) -> Result<Value> {
    if held.is_empty() {
        // Nothing to be in the middle of, and zero would be a claim — the rule
        // `mean` already follows over an empty group.
        return Ok(Value::None);
    }
    let mut sorted = Vec::with_capacity(held.len());
    for value in held {
        sorted.push(exact(value, span)?);
    }
    sorted.sort_unstable();
    let at = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        return Ok(sorted.get(at).map_or(Value::None, exactly));
    }
    let (Some(lower), Some(upper)) = (sorted.get(at.saturating_sub(1)), sorted.get(at)) else {
        return Ok(Value::None);
    };
    let pair = lower
        .checked_add(*upper)
        .ok_or_else(|| failed("median", "a total outside the exact range", span))?;
    let averaged = pair
        .checked_div(Decimal::from(2))
        .ok_or_else(|| failed("median", "a group of no size", span))?;
    Ok(exactly(&averaged))
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

/// The number this value offers a numeric fold, or nothing when it offers none.
///
/// Absent and null are both "no value here" and every numeric fold passes over
/// them; anything else present that is not a number is refused, which is where
/// the batch fold refuses it too.
fn summable<'a>(value: &'a Value, fold: &'static str, span: Span) -> Result<Option<&'a Number>> {
    if !present(value) {
        return Ok(None);
    }
    let Value::Number(number) = value else {
        return Err(Error::NotSummable {
            fold,
            found: value.type_name(),
            span,
        });
    };
    Ok(Some(number))
}

/// Add one number to an exact running total, remembering a failure rather than
/// raising it.
fn add_exact(running: &mut Running<Decimal>, number: &Number) {
    let Ok(total) = running else {
        return;
    };
    let next = number
        .as_decimal()
        .ok_or("a number outside the exact range")
        .and_then(|held| {
            total
                .checked_add(held)
                .ok_or("a total outside the exact range")
        });
    *running = next;
}

/// Add one number to a float running total, in the order the values arrived.
fn add_float(running: &mut Running<f64>, number: &Number) {
    let Ok(total) = running else {
        return;
    };
    let Some(held) = approximate(number) else {
        *running = Err("a number no float can hold");
        return;
    };
    *total += held;
}

/// A failure a total carried until it turned out to be the answer.
fn failed(fold: &'static str, found: &'static str, span: Span) -> Error {
    Error::NotSummable { fold, found, span }
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a unit test that cannot fail loudly is the failure this module's own history is about"
)]
mod tests {
    use super::{Accumulator, Aggregate, Decimal, Number, Retention, Span, Value};
    use crate::aggregate::fold;

    /// A span the corpus can share; no assertion reads it.
    fn span() -> Span {
        Span::new(0, 0)
    }

    /// Fold these values the way the executor now does — one at a time.
    fn incrementally(aggregate: Aggregate, values: &[Value]) -> crate::error::Result<Value> {
        let mut accumulator = Accumulator::for_aggregate(aggregate, span());
        for value in values {
            accumulator.offer(value)?;
        }
        accumulator.finish()
    }

    /// Every aggregate this store has.
    const EVERY: &[Aggregate] = &[
        Aggregate::Count,
        Aggregate::Sum,
        Aggregate::Mean,
        Aggregate::Min,
        Aggregate::Max,
        Aggregate::Variance,
        Aggregate::Stddev,
        Aggregate::Median,
        Aggregate::Collect,
    ];

    /// A number as a value, spelled once.
    fn integer(held: i64) -> Value {
        Value::Number(Number::Integer(held))
    }

    /// A float as a value.
    fn float(held: f64) -> Value {
        Value::Number(Number::float(held))
    }

    /// An exact decimal as a value.
    fn decimal(held: &str) -> Value {
        Value::Number(Number::Decimal(held.parse::<Decimal>().unwrap()))
    }

    /// The largest exact number there is, so that two of them cannot be added.
    ///
    /// Measured rather than assumed: the first version of the deferred-failure
    /// tests used `i64::MAX` twice, which a decimal holds without complaint —
    /// so they exercised no failure at all and the falsification that raises one
    /// eagerly passed straight through them.
    fn beyond_exact() -> Value {
        Value::Number(Number::Decimal(Decimal::MAX))
    }

    /// Groups a fold has to answer the same way either way.
    ///
    /// Deliberately mixed: kinds that promote differently, values a fold passes
    /// over, an order that decides a tie, a group of one and a group of none.
    /// The float rows are the ones the design note is about — a total that
    /// switched kinds part-way would differ from these in the last bits.
    fn corpus() -> Vec<(&'static str, Vec<Value>)> {
        vec![
            ("nothing at all", vec![]),
            ("one integer", vec![integer(7)]),
            ("integers", vec![integer(3), integer(-9), integer(12)]),
            (
                "integers and a decimal",
                vec![integer(3), decimal("0.25"), integer(4)],
            ),
            (
                "a float last, after exact numbers",
                vec![integer(1), decimal("2.5"), float(0.1)],
            ),
            (
                "a float first, before exact numbers",
                vec![float(0.1), integer(1), decimal("2.5")],
            ),
            (
                "floats that do not add associatively",
                vec![
                    float(0.1),
                    float(0.2),
                    float(0.3),
                    float(1e16),
                    float(-1e16),
                ],
            ),
            (
                "values a fold passes over",
                vec![Value::None, integer(5), Value::Null, integer(5)],
            ),
            ("only values it passes over", vec![Value::None, Value::Null]),
            (
                "equal values of one kind, which a tie cannot tell apart",
                vec![integer(4), integer(4), integer(4)],
            ),
            (
                "values that compare equal and are not the same value",
                // `3`, `3.0` and the decimal `3.0` all order equal, so which one
                // an extreme answers with is decided entirely by whether it
                // replaces on a tie — and the two ends decide it oppositely.
                // Only the structural comparison above can see the difference;
                // measured, not assumed, because the first version of this row
                // asserted value equality and the falsification that reverses
                // the tie rule passed straight through it.
                vec![integer(3), float(3.0), decimal("3.0")],
            ),
            (
                "integers, whose total must still answer as an integer",
                // Guards the promotion, which value equality also cannot see:
                // `Integer(6)` and `Decimal(6)` are equal values and different
                // answers on the wire.
                vec![integer(1), integer(2), integer(3)],
            ),
            (
                "strings, which only the counting folds accept",
                vec![
                    Value::String("b".to_owned()),
                    Value::String("a".to_owned()),
                    Value::String("c".to_owned()),
                ],
            ),
            (
                "kinds the value system orders against each other",
                vec![integer(1), Value::Bool(true), Value::String("a".to_owned())],
            ),
            (
                "an integer total no i64 holds",
                vec![integer(i64::MAX), integer(i64::MAX)],
            ),
            (
                "a total no exact number holds",
                vec![beyond_exact(), beyond_exact()],
            ),
            (
                "a total no exact number holds, with a float after it",
                vec![beyond_exact(), beyond_exact(), float(2.0)],
            ),
        ]
    }

    /// Whether two answers are the same spread computed two different ways.
    ///
    /// `variance` and `stddev` are the only folds whose reference is a
    /// **different algorithm** rather than a different arrangement of the same
    /// arithmetic — two passes against Welford's recurrence — and two float
    /// algorithms do not agree in the last bits. Demanding that they did would
    /// force the oracle to become a copy of the implementation, which proves
    /// nothing; so these two are compared within a relative tolerance and every
    /// other fold keeps the strict structural comparison below.
    ///
    /// The tolerance is relative and tight: a sign error, a population divisor
    /// where a sample one belongs, or a missing square root are all changes of
    /// several percent or more on this corpus, and none of them survives it.
    fn same_spread(batch: &Value, running: &Value) -> bool {
        const TOLERANCE: f64 = 1e-9;
        match (batch, running) {
            (Value::Number(Number::Float(left)), Value::Number(Number::Float(right))) => {
                let scale = left.abs().max(right.abs()).max(1.0);
                (left - right).abs() / scale < TOLERANCE
            }
            (left, right) => format!("{left:?}") == format!("{right:?}"),
        }
    }

    #[test]
    fn folding_one_value_at_a_time_answers_what_folding_them_all_at_once_answers() {
        for aggregate in EVERY {
            for (what, values) in corpus() {
                let batch = fold(*aggregate, &values, span());
                let running = incrementally(*aggregate, &values);
                if matches!(aggregate, Aggregate::Variance | Aggregate::Stddev)
                    && let (Ok(batch), Ok(running)) = (&batch, &running)
                {
                    assert!(
                        same_spread(batch, running),
                        "{aggregate:?} over {what} answered {batch:?} in one pass and \
                         {running:?} incrementally"
                    );
                    continue;
                }
                match (batch, running) {
                    // Compared **structurally**, not by value equality. This
                    // store's `Value` deliberately orders and equates numbers
                    // across kinds — `3`, `3.0` and the decimal `3.0` are all
                    // equal, which is what lets a join match what `=` matches.
                    // So `assert_eq!` on the values would pass a `sum` that
                    // stopped promoting to an integer, and pass an extreme that
                    // kept the wrong end of a tie between two kinds. Both are
                    // differences a caller sees on the wire, and both are
                    // invisible to the comparison this test would otherwise
                    // make.
                    (Ok(batch), Ok(running)) => assert_eq!(
                        format!("{batch:?}"),
                        format!("{running:?}"),
                        "{aggregate:?} over {what} answered differently"
                    ),
                    (Err(batch), Err(running)) => assert_eq!(
                        format!("{batch}"),
                        format!("{running}"),
                        "{aggregate:?} over {what} failed differently"
                    ),
                    (batch, running) => panic!(
                        "{aggregate:?} over {what}: one answered and the other did not — \
                         {batch:?} against {running:?}"
                    ),
                }
            }
        }
    }

    /// The spread folds against numbers whose answer is known independently.
    ///
    /// The equivalence test above compares two implementations to each other,
    /// which cannot catch a mistake they share — a population divisor in both
    /// would pass it. These are hand-checked: `[2, 4, 4, 4, 5, 5, 7, 9]` has a
    /// sample variance of `32/7` and a population variance of `4`, and its
    /// population standard deviation is exactly `2`, so the three plausible
    /// wrong answers are all far outside the tolerance.
    #[test]
    fn the_spread_is_the_sample_form_and_not_the_population_one() {
        let values: Vec<Value> = [2, 4, 4, 4, 5, 5, 7, 9]
            .iter()
            .map(|n| integer(*n))
            .collect();

        let Ok(Value::Number(Number::Float(variance))) =
            incrementally(Aggregate::Variance, &values)
        else {
            panic!("variance did not answer a float")
        };
        assert!(
            (variance - 32.0 / 7.0).abs() < 1e-9,
            "variance answered {variance}, which is the population form (4.0) if it is 4"
        );

        let Ok(Value::Number(Number::Float(deviation))) = incrementally(Aggregate::Stddev, &values)
        else {
            panic!("stddev did not answer a float")
        };
        assert!(
            (deviation - (32.0_f64 / 7.0).sqrt()).abs() < 1e-9,
            "stddev answered {deviation}; the population form is exactly 2 here"
        );
    }

    /// Welford earns its place against the form it replaced.
    ///
    /// `E[x²] − E[x]²` over these three values subtracts two numbers that agree
    /// to fifteen significant digits and answers `0` — or a negative number,
    /// whose square root is not a number at all. The true variance is `1`. This
    /// is the whole reason the algorithm is not the one-line textbook version,
    /// so it is pinned rather than left as a remark in a comment.
    #[test]
    fn the_spread_survives_numbers_the_textbook_form_cancels_away() {
        let values = vec![float(1e9 + 4.0), float(1e9 + 7.0), float(1e9 + 13.0)];
        let Ok(Value::Number(Number::Float(variance))) =
            incrementally(Aggregate::Variance, &values)
        else {
            panic!("variance did not answer a float")
        };
        assert!(
            (variance - 21.0).abs() < 1e-6,
            "variance answered {variance}; the deviations are -4, -1 and 5 about a \
             mean of 1e9+8, so the sample variance is 42/2 = 21"
        );
    }

    /// The two ends of `median`, and the empty group.
    #[test]
    fn the_middle_is_the_middle_number_and_the_mean_of_two_when_there_are_two() {
        let odd = incrementally(Aggregate::Median, &[integer(9), integer(1), integer(5)]).unwrap();
        assert_eq!(format!("{odd:?}"), format!("{:?}", decimal("5")));

        let even = incrementally(
            Aggregate::Median,
            &[integer(9), integer(1), integer(5), integer(3)],
        )
        .unwrap();
        assert_eq!(format!("{even:?}"), format!("{:?}", decimal("4")));

        assert_eq!(incrementally(Aggregate::Median, &[]).unwrap(), Value::None);
    }

    /// The property the exact-and-normalised answer was chosen for.
    ///
    /// Three values that compare **equal** and are three different answers on
    /// the wire. Answering with the middle value as written makes the result
    /// depend on where the sort left them, which is not a property of the data;
    /// the corpus row that says so is what caught the first version of this
    /// fold. Here the same multiset is offered in every order it has, and the
    /// answer has to be one answer.
    #[test]
    fn the_middle_does_not_depend_on_the_order_the_records_arrived_in() {
        let equal = [integer(3), float(3.0), decimal("3.0")];
        let orders = [
            [0_usize, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let mut answers: Vec<String> = Vec::new();
        for order in orders {
            let offered: Vec<Value> = order
                .iter()
                .map(|at| equal.get(*at).unwrap().clone())
                .collect();
            let answered = incrementally(Aggregate::Median, &offered).unwrap();
            answers.push(format!("{answered:?}"));
        }
        answers.dedup();
        assert_eq!(
            answers.len(),
            1,
            "the same three numbers gave more than one median: {answers:?}"
        );
    }

    /// `collect` answers an empty array and never `NONE`.
    ///
    /// The rule `sum` follows for the same reason: an answer every caller has to
    /// write `?? []` after is the wrong answer.
    #[test]
    fn collecting_nothing_answers_an_empty_array() {
        assert_eq!(
            incrementally(Aggregate::Collect, &[]).unwrap(),
            Value::Array(Vec::new())
        );
        assert_eq!(
            incrementally(Aggregate::Collect, &[Value::None, integer(3), Value::Null]).unwrap(),
            Value::Array(vec![integer(3)]),
            "collect skipped nothing, where every other fold passes over absent values"
        );
    }

    /// The claim this module exists for, now asserted per class.
    ///
    /// Weakening it to `held() <= n` when `collect` and `median` arrived would
    /// have deleted the guarantee for the seven folds that still have it. So the
    /// exception is **named** — [`Aggregate::retention`] — and the assertion
    /// splits along it.
    #[test]
    fn a_constant_space_fold_holds_at_most_one_value_however_many_it_is_offered() {
        const OFFERED: i64 = 50_000;
        for aggregate in EVERY
            .iter()
            .filter(|fold| fold.retention() == Retention::Constant)
        {
            let mut accumulator = Accumulator::for_aggregate(*aggregate, span());
            for held in 0..OFFERED {
                accumulator.offer(&integer(held)).unwrap();
                assert!(
                    accumulator.held() <= 1,
                    "{aggregate:?} held {} values after {} offers",
                    accumulator.held(),
                    held.saturating_add(1)
                );
            }
            // The answer is still right, so the retention is not bought by
            // dropping what it was offered.
            assert!(
                accumulator.finish().is_ok(),
                "{aggregate:?} could not answer"
            );
        }
    }

    /// The other half, which is what stops the classification being an escape.
    ///
    /// A fold could be listed as whole-group and quietly hold nothing, and the
    /// test above would pass while the memory ceiling refused reads for a cost
    /// they no longer pay. So a whole-group fold is required to actually hold
    /// its group: the label has to be *earned* in both directions.
    #[test]
    fn a_whole_group_fold_holds_exactly_what_it_was_offered() {
        const OFFERED: i64 = 1_000;
        for aggregate in EVERY
            .iter()
            .filter(|fold| fold.retention() == Retention::WholeGroup)
        {
            let mut accumulator = Accumulator::for_aggregate(*aggregate, span());
            for held in 0..OFFERED {
                accumulator.offer(&integer(held)).unwrap();
            }
            assert_eq!(
                accumulator.held(),
                usize::try_from(OFFERED).unwrap(),
                "{aggregate:?} is classified as holding its group and does not"
            );
            assert!(
                accumulator.finish().is_ok(),
                "{aggregate:?} could not answer"
            );
        }
    }

    /// Every fold is exercised by the corpus tests, which `EVERY` is the list for.
    ///
    /// `EVERY` is written out rather than aliased to `Aggregate::ALL` so that a
    /// reader sees the set; this keeps the two from drifting, which is the only
    /// way a new fold could reach the store untested by any of the above.
    #[test]
    fn the_corpus_exercises_every_fold_the_language_has() {
        assert_eq!(EVERY, Aggregate::ALL);
    }

    #[test]
    fn a_deferred_failure_is_discarded_when_the_other_total_is_the_answer() {
        // The exact total goes out of range on the second value, and a float
        // arrives after it — so the answer comes from the float total and the
        // exact failure is never read.
        let values = vec![beyond_exact(), beyond_exact(), float(1.0)];
        let running = incrementally(Aggregate::Sum, &values).unwrap();
        assert!(
            matches!(running, Value::Number(Number::Float(_))),
            "a float in the group must make the answer a float, got {running:?}"
        );
        assert_eq!(running, fold(Aggregate::Sum, &values, span()).unwrap());
    }

    #[test]
    fn a_value_of_the_wrong_kind_outranks_a_number_out_of_range() {
        // The out-of-range number comes first and the string second; the batch
        // fold type-checks the whole group before any arithmetic, so it reports
        // the string. Deferring the arithmetic failure is what keeps that true.
        let values = vec![
            beyond_exact(),
            beyond_exact(),
            Value::String("not a number".to_owned()),
        ];
        let batch = fold(Aggregate::Sum, &values, span()).unwrap_err();
        let running = incrementally(Aggregate::Sum, &values).unwrap_err();
        assert_eq!(format!("{batch}"), format!("{running}"));
    }
}
