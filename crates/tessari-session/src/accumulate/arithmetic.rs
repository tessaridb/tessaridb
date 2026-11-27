//! The arithmetic folds share: running sums and the two moments a spread needs.

use super::exact::ExactSum;
use super::{Running, exact, exactly};
use crate::aggregate::{approximate, present};
use crate::error::{Error, Result};
use rust_decimal::Decimal;
use tessari_ql::Span;
use tessari_types::{Number, Value};

/// What a spread needs: a count, the exact sum of the numbers and the exact sum
/// of their squares (ADR-0114 D4).
///
/// The textbook `E[x²] − E[x]²` form loses every significant digit of the
/// answer when it is computed in floats on data whose spread is small next to
/// its magnitude — timestamps and prices, which is most of what anybody takes a
/// variance of. Here the subtraction is made on exact totals and rounded once,
/// so there is nothing to lose; and exact totals add in any order, which is what
/// lets a spread merge across shards into the bits one walk gives.
///
/// The numbers enter as the floats they convert to, because `stddev` takes a
/// square root and no square root is exact. A `variance` that promised more
/// than the float it answers while the `stddev` beside it could not would be
/// two answers with two different promises out of one pair of folds.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Moments {
    /// How many numbers went in.
    pub(crate) counted: u64,
    /// Their exact sum.
    pub(crate) total: ExactSum,
    /// The exact sum of their squares.
    pub(crate) squares: ExactSum,
}

impl Moments {
    /// Fold one more number in.
    pub(crate) fn offer(&mut self, held: f64) {
        self.counted = self.counted.saturating_add(1);
        self.total.add(held);
        self.squares.add_product(held, held);
    }

    /// Fold in the moments another walk reached.
    pub(crate) fn absorb(&mut self, other: &Self) {
        self.counted = self.counted.saturating_add(other.counted);
        self.total.absorb(&other.total);
        self.squares.absorb(&other.squares);
    }

    /// The sample variance, or `None` when fewer than two numbers arrived.
    ///
    /// `NONE` rather than zero over one value, by the same rule `mean` follows
    /// over none: the spread of a single observation is not zero, it is a
    /// question nobody has enough data to answer, and zero would be a claim.
    ///
    /// # Errors
    ///
    /// When a square or a total went past what a float holds.
    pub(crate) fn variance(&self) -> core::result::Result<Option<f64>, &'static str> {
        if self.counted < 2 {
            return Ok(None);
        }
        if self.total.has_special() || self.squares.has_special() {
            // An infinity or a not-a-number has no spread around it.
            return Ok(Some(f64::NAN));
        }
        // n·Σx² − (Σx)², formed exactly: the count enters as two halves that
        // are each a float exactly, so no part of the product is rounded.
        let (high, low) = halves(self.counted);
        let mut numerator = self.squares.scaled(high);
        numerator.absorb(&self.squares.scaled(low));
        numerator.absorb(&self.total.squared().negated());
        let numerator = numerator
            .total()
            .map_err(|_| "a spread outside the float range")?;
        let counted = count(self.counted);
        let degrees = counted * count(self.counted.saturating_sub(1));
        // Never below zero when exact; a square that fell under the smallest
        // float lost bits, and a spread that rounding left negative is none.
        Ok(Some(numerator.max(0.0) / degrees))
    }
}

/// The running sums a paired fold answers from — the bivariate sibling of
/// [`Moments`], for `corr`, `covar` and the three `regr_` folds.
///
/// The update is the stable co-moment form: each pair adds one product of two
/// floats to each exact total, which is the operation [`ExactSum`] holds
/// exactly. The textbook alternative — `n·Σxy − Σx·Σy` over raw sums — needs
/// the product of two *sums*, which has no exact form, and it cancels exactly
/// where the data is nearly a line, which is where these folds are asked.
/// Every answer is a float, by `stddev`'s rule: no answer here promises more
/// than the float it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Moments2 {
    /// How many pairs went in.
    pub(crate) counted: u64,
    /// The mean of the `x` side so far.
    pub(crate) mean_x: f64,
    /// The mean of the `y` side so far.
    pub(crate) mean_y: f64,
    /// Σ(x−x̄)(y−ȳ), the co-moment.
    pub(crate) co: ExactSum,
    /// Σ(x−x̄)².
    pub(crate) xx: ExactSum,
    /// Σ(y−ȳ)².
    pub(crate) yy: ExactSum,
}

impl Moments2 {
    /// Fold one more pair in.
    pub(crate) fn offer(&mut self, x: f64, y: f64) {
        let counted = self.counted.saturating_add(1);
        let step = count(counted);
        let dx = x - self.mean_x;
        let dy = y - self.mean_y;
        self.mean_x += dx / step;
        self.mean_y += dy / step;
        // x − x̄_new is dx·(n−1)/n, so each product below is the textbook
        // update's, formed as the one product an exact total holds exactly.
        self.co.add_product(dx, y - self.mean_y);
        self.xx.add_product(dx, x - self.mean_x);
        self.yy.add_product(dy, y - self.mean_y);
        self.counted = counted;
    }

    /// Fold in the sums another walk reached — the parallel form of the same
    /// update: the cross term `dx·dy·na·nb/n` is what the two walks' means
    /// moving to the joint mean would have added.
    pub(crate) fn absorb(&mut self, other: &Self) {
        if other.counted == 0 {
            return;
        }
        if self.counted == 0 {
            *self = other.clone();
            return;
        }
        let joint = count(self.counted.saturating_add(other.counted));
        let mine = count(self.counted);
        let theirs = count(other.counted);
        let dx = other.mean_x - self.mean_x;
        let dy = other.mean_y - self.mean_y;
        let weigh = (mine * theirs) / joint;
        self.co.absorb(&other.co);
        self.co.add_product(dx * dy, weigh);
        self.xx.absorb(&other.xx);
        self.xx.add_product(dx * dx, weigh);
        self.yy.absorb(&other.yy);
        self.yy.add_product(dy * dy, weigh);
        self.mean_x = (self.mean_x * mine + other.mean_x * theirs) / joint;
        self.mean_y = (self.mean_y * mine + other.mean_y * theirs) / joint;
        self.counted = self.counted.saturating_add(other.counted);
    }

    /// The three totals, or nothing under two pairs; an infinity or a
    /// not-a-number anywhere answers `NaN`, by `variance`'s rule.
    fn totals(&self) -> core::result::Result<Option<(f64, f64, f64)>, &'static str> {
        if self.counted < 2 {
            return Ok(None);
        }
        if self.co.has_special() || self.xx.has_special() || self.yy.has_special() {
            return Ok(Some((f64::NAN, f64::NAN, f64::NAN)));
        }
        let co = self
            .co
            .total()
            .map_err(|_| "a co-moment outside the float range")?;
        let xx = self
            .xx
            .total()
            .map_err(|_| "a spread outside the float range")?;
        let yy = self
            .yy
            .total()
            .map_err(|_| "a spread outside the float range")?;
        Ok(Some((co, xx.max(0.0), yy.max(0.0))))
    }

    /// The sample covariance, or `None` under two pairs — `variance`'s rule:
    /// the spread of one pair is not zero, it is unasked.
    ///
    /// # Errors
    ///
    /// When a total went past what a float holds.
    pub(crate) fn covariance(&self) -> core::result::Result<Option<f64>, &'static str> {
        let Some((co, _, _)) = self.totals()? else {
            return Ok(None);
        };
        Ok(Some(co / count(self.counted.saturating_sub(1))))
    }

    /// The Pearson correlation, or `None` where a side never moves: over a
    /// constant there is no correlation, and zero would be a claim.
    ///
    /// # Errors
    ///
    /// When a total went past what a float holds.
    pub(crate) fn correlation(&self) -> core::result::Result<Option<f64>, &'static str> {
        let Some((co, xx, yy)) = self.totals()? else {
            return Ok(None);
        };
        if xx == 0.0 || yy == 0.0 {
            return Ok(None);
        }
        // Rounding can carry the ratio a step past ±1; the correlation of
        // anything is inside it, so the step is clamped rather than answered.
        Ok(Some((co / (xx * yy).sqrt()).clamp(-1.0, 1.0)))
    }

    /// The least-squares slope of `y` over `x`, or `None` where `x` never
    /// moves — a vertical stack of points names no line.
    ///
    /// # Errors
    ///
    /// When a total went past what a float holds.
    pub(crate) fn slope(&self) -> core::result::Result<Option<f64>, &'static str> {
        let Some((co, xx, _)) = self.totals()? else {
            return Ok(None);
        };
        if xx == 0.0 {
            return Ok(None);
        }
        Ok(Some(co / xx))
    }

    /// Where the least-squares line crosses `x = 0`, or `None` wherever the
    /// slope is.
    ///
    /// # Errors
    ///
    /// When a total went past what a float holds.
    pub(crate) fn intercept(&self) -> core::result::Result<Option<f64>, &'static str> {
        let Some(slope) = self.slope()? else {
            return Ok(None);
        };
        Ok(Some(self.mean_y - slope * self.mean_x))
    }

    /// The coefficient of determination of the least-squares line — the
    /// square of the correlation — or `None` wherever the correlation is.
    ///
    /// # Errors
    ///
    /// When a total went past what a float holds.
    pub(crate) fn r2(&self) -> core::result::Result<Option<f64>, &'static str> {
        Ok(self.correlation()?.map(|held| held * held))
    }
}

/// The running shape sums `skewness` and `kurtosis` answer from — the
/// one-pass central-moment update (Pébay's recurrence), whose every step is
/// a short chain of float products. The chain is rounded where the exact
/// sums elsewhere are not, and that is declared: both answers are floats by
/// `stddev`'s rule, and the alternative — raw power sums — cancels exactly
/// where the question is asked, on data whose shape is small next to its
/// magnitude.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Shape {
    /// How many numbers went in.
    pub(crate) counted: u64,
    /// The mean so far.
    mean: f64,
    /// The second central moment's running sum.
    m2: f64,
    /// The third's.
    m3: f64,
    /// The fourth's.
    m4: f64,
}

impl Shape {
    /// Fold one more number in.
    pub(crate) fn offer(&mut self, held: f64) {
        let earlier = count(self.counted);
        self.counted = self.counted.saturating_add(1);
        let step = count(self.counted);
        let delta = held - self.mean;
        let along = delta / step;
        let along2 = along * along;
        let swing = delta * along * earlier;
        self.mean += along;
        self.m4 += swing * along2 * (step * step - 3.0 * step + 3.0) + 6.0 * along2 * self.m2
            - 4.0 * along * self.m3;
        self.m3 += swing * along * (step - 2.0) - 3.0 * along * self.m2;
        self.m2 += swing;
    }

    /// The adjusted Fisher–Pearson skewness, or `None` under three numbers:
    /// the sample form divides by `n − 2`, and a shape one value cannot give
    /// is not zero, it is unasked.
    pub(crate) fn skewness(&self) -> Option<f64> {
        if self.counted < 3 || self.m2 <= 0.0 {
            return None;
        }
        let n = count(self.counted);
        Some((n * (n - 1.0)).sqrt() / (n - 2.0) * (self.m3 / self.m2.powf(1.5)))
    }

    /// The sample excess kurtosis, or `None` under four numbers: the sample
    /// form divides by `(n − 2)(n − 3)`. Zero is a normal shape's answer,
    /// positive a heavier tail's, negative a flatter one's.
    pub(crate) fn kurtosis(&self) -> Option<f64> {
        if self.counted < 4 || self.m2 <= 0.0 {
            return None;
        }
        let n = count(self.counted);
        let plain = n * self.m4 / (self.m2 * self.m2);
        Some((n - 1.0) / ((n - 2.0) * (n - 3.0)) * ((n + 1.0) * (plain - 3.0) + 6.0))
    }
}

/// A count as two floats, each exact, that add to it: the high 32 bits scaled
/// and the low 32 bits.
fn halves(counted: u64) -> (f64, f64) {
    let high = u32::try_from(counted >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(counted & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    (f64::from(high) * 4_294_967_296.0, f64::from(low))
}

/// A count as the nearest float — exact up to 2^53, rounded past it.
pub(crate) fn count(counted: u64) -> f64 {
    let (high, low) = halves(counted);
    high + low
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
pub(crate) fn middle(
    held: &[Value],
    runs: &std::collections::BTreeMap<Decimal, u64>,
    span: Span,
) -> Result<Value> {
    let mut counted = runs.clone();
    for value in held {
        let entry = counted.entry(exact(value, span)?).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    let size: u64 = counted
        .values()
        .fold(0, |size, many| size.saturating_add(*many));
    if size == 0 {
        // Nothing to be in the middle of, and zero would be a claim — the rule
        // `mean` already follows over an empty group.
        return Ok(Value::None);
    }
    let at = size / 2;
    let upper = ranked(&counted, at);
    if size % 2 == 1 {
        return Ok(upper.map_or(Value::None, exactly));
    }
    let (Some(lower), Some(upper)) = (ranked(&counted, at.saturating_sub(1)), upper) else {
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

/// The values at several ranks of the same multiset, exactly — the value at
/// index ⌊rank × (n − 1)⌋ of the sorted values, which is the definition
/// `approx_quantile`'s sketch estimates. Over nothing, `NONE` by `mean`'s
/// rule; the answer is an array in the ranks' order, normalized like
/// `median`'s so the same multiset answers the same whatever order the
/// records arrived in.
pub(crate) fn exact_ranks(
    held: &[Value],
    runs: &std::collections::BTreeMap<Decimal, u64>,
    ranks: &[f64],
    span: Span,
) -> Result<Value> {
    let mut counted = runs.clone();
    for value in held {
        let entry = counted.entry(exact(value, span)?).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    let size: u64 = counted
        .values()
        .fold(0, |size, many| size.saturating_add(*many));
    if size == 0 {
        return Ok(Value::None);
    }
    let last = count(size.saturating_sub(1));
    let mut answered = Vec::with_capacity(ranks.len());
    for rank in ranks {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let at = (rank * last).floor().max(0.0) as u64;
        let held = ranked(&counted, at.min(size.saturating_sub(1))).map_or(Value::None, exactly);
        answered.push(held);
    }
    Ok(Value::Array(answered))
}

/// The Gini coefficient of the multiset — zero is perfect equality, one is
/// one value holding everything — summed over the sorted runs in exact
/// decimals: Σ i·xᵢ over 1-based positions, then one division. `NONE` over
/// nothing and over all zeros, where equality has no denominator.
pub(crate) fn gini(
    held: &[Value],
    runs: &std::collections::BTreeMap<Decimal, u64>,
    span: Span,
) -> Result<Value> {
    let mut counted = runs.clone();
    for value in held {
        let entry = counted.entry(exact(value, span)?).or_insert(0);
        *entry = entry.saturating_add(1);
    }
    let mut total = Decimal::ZERO;
    let mut weighted = Decimal::ZERO;
    let mut seen: u64 = 0;
    for (value, many) in &counted {
        // The run occupies the 1-based positions seen+1 … seen+many, whose
        // sum is many·(2·seen + many + 1)/2.
        let run = Decimal::from(*many);
        let span_sum = Decimal::from(
            seen.saturating_mul(2)
                .saturating_add(*many)
                .saturating_add(1),
        );
        let positions = run
            .checked_mul(span_sum)
            .and_then(|held| held.checked_div(Decimal::from(2)))
            .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
        weighted = weighted
            .checked_add(
                value
                    .checked_mul(positions)
                    .ok_or_else(|| failed("gini", "a total outside the exact range", span))?,
            )
            .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
        total = total
            .checked_add(
                value
                    .checked_mul(run)
                    .ok_or_else(|| failed("gini", "a total outside the exact range", span))?,
            )
            .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
        seen = seen.saturating_add(*many);
    }
    if seen == 0 || total == Decimal::ZERO {
        return Ok(Value::None);
    }
    // G = (2·Σ i·xᵢ − (n+1)·Σx) / (n·Σx).
    let twice = weighted
        .checked_mul(Decimal::from(2))
        .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
    let lift = Decimal::from(seen.saturating_add(1))
        .checked_mul(total)
        .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
    let numerator = twice
        .checked_sub(lift)
        .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
    let denominator = Decimal::from(seen)
        .checked_mul(total)
        .ok_or_else(|| failed("gini", "a total outside the exact range", span))?;
    let answered = numerator
        .checked_div(denominator)
        .ok_or_else(|| failed("gini", "a coefficient no decimal holds", span))?;
    Ok(Value::Number(Number::Decimal(answered.normalize())))
}

/// The numbers in `bins` equal-width bins from the smallest to the largest,
/// as `{ from, to, count }` per bin with the last bin holding its top edge;
/// `NONE` over nothing, where no extent names a bin. The arithmetic is
/// float, by `stddev`'s rule: the edges of an equal-width split are floats
/// whatever the values were.
pub(crate) fn histogram(held: &[f64], bins: u64) -> Value {
    if held.is_empty() {
        return Value::None;
    }
    let bins = usize::try_from(bins).unwrap_or(usize::MAX).max(1);
    let low = held.iter().copied().fold(f64::INFINITY, f64::min);
    let high = held.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let width = (high - low) / count(u64::try_from(bins).unwrap_or(u64::MAX));
    let mut counted = vec![0_u64; bins];
    for value in held {
        let at = if width > 0.0 {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let at = ((value - low) / width).floor().max(0.0) as usize;
            at.min(bins.saturating_sub(1))
        } else {
            0
        };
        counted[at] = counted[at].saturating_add(1);
    }
    let answered: Vec<Value> = counted
        .iter()
        .enumerate()
        .map(|(at, many)| {
            let from = low + width * count(u64::try_from(at).unwrap_or(u64::MAX));
            let to = if at + 1 == bins {
                high
            } else {
                low + width * count(u64::try_from(at).unwrap_or(u64::MAX).saturating_add(1))
            };
            let mut held = std::collections::BTreeMap::new();
            held.insert(
                "count".to_string(),
                Value::Number(Number::Integer(i64::try_from(*many).unwrap_or(i64::MAX))),
            );
            held.insert("from".to_string(), Value::Number(Number::float(from)));
            held.insert("to".to_string(), Value::Number(Number::float(to)));
            Value::Object(held)
        })
        .collect();
    Value::Array(answered)
}

/// The value at zero-based `rank` of a multiset held as value → how many.
fn ranked(counted: &std::collections::BTreeMap<Decimal, u64>, rank: u64) -> Option<&Decimal> {
    let mut below = 0_u64;
    for (value, many) in counted {
        below = below.saturating_add(*many);
        if rank < below {
            return Some(value);
        }
    }
    None
}

/// The number this value offers a numeric fold, or nothing when it offers none.
///
/// Absent and null are both "no value here" and every numeric fold passes over
/// them; anything else present that is not a number is refused, which is where
/// the batch fold refuses it too.
pub(crate) fn summable<'a>(
    value: &'a Value,
    fold: &'static str,
    span: Span,
) -> Result<Option<&'a Number>> {
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
pub(crate) fn add_exact(running: &mut Running<Decimal>, number: &Number) {
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

/// Add one number to an exact float total, remembering a failure rather than
/// raising it.
pub(crate) fn add_float(running: &mut Running<ExactSum>, number: &Number) {
    let Ok(total) = running else {
        return;
    };
    let Some(held) = approximate(number) else {
        *running = Err("a number no float can hold");
        return;
    };
    total.add(held);
}

/// A failure a total carried until it turned out to be the answer.
pub(crate) fn failed(fold: &'static str, found: &'static str, span: Span) -> Error {
    Error::NotSummable { fold, found, span }
}
