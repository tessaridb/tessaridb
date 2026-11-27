/// A fold over the records of a group.
///
/// Spelled without a namespace, where every function has one. That is the
/// namespacing rule earning its keep rather than being broken by it:
/// `array::len` counts one record's array and `count` counts records, and a
/// reader can tell which arity they are looking at from the spelling alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// `count(*)` counts records; `count(<expr>)` counts the records where the
    /// expression is present and not null.
    Count,
    /// `sum(<expr>)` — over nothing, zero.
    Sum,
    /// `mean(<expr>)` — over nothing, `NONE`.
    Mean,
    /// `min(<expr>)`, in the value system's order.
    Min,
    /// `max(<expr>)`, in the same order.
    Max,
    /// `variance(<expr>)` — the **sample** variance, dividing by `n − 1`.
    ///
    /// Sample rather than population because a table's rows are usually a
    /// sample of something, which is why the SQL standard's `VARIANCE` is the
    /// sample form `VAR_SAMP`. The population form
    /// is not a second name because the language can already say it:
    /// `variance(x) * (count(x) - 1) / count(x)`.
    ///
    /// Over fewer than two numbers, `NONE` — `n − 1` is zero there, and the
    /// spread of one value is not zero, it is unasked.
    Variance,
    /// `stddev(<expr>)` — the square root of [`Self::Variance`], and sample for
    /// the same reason.
    Stddev,
    /// `median(<expr>)` — the middle number, or the mean of the two middles.
    ///
    /// Numeric like `mean`, and refusing anything else for the same reason. An
    /// even count answers the mean of the two middles, which is a value that
    /// was never in the data — acceptable only because the fold is numeric; the
    /// same rule over a `datetime` or a `uuid` column would have to construct a
    /// value of a kind that has no arithmetic.
    Median,
    /// `increase(<value>, <instant>)` — the sum of the rises between samples
    /// ordered by the instant, a fall counting as a counter reset.
    Increase,
    /// `rate(<value>, <instant>)` — [`Self::Increase`] per second between the
    /// first and last sample.
    Rate,
    /// `delta(<value>, <instant>)` — last minus first, with no reset handling.
    Delta,
    /// `resets(<value>, <instant>)` — how many times the samples, ordered by
    /// the instant, fall between one and the next: the counter resets the
    /// window saw. A window of one sample saw none, which is a complete
    /// answer rather than an absent one — unlike `increase`, whose total is
    /// genuinely unknown there.
    Resets,
    /// `deriv(<value>, <instant>)` — the least-squares slope of the samples
    /// per second, as a float. A window with fewer than two samples, or all
    /// at one instant, names no slope and answers `NONE`.
    Deriv,
    /// `predict_linear(<value>, <instant>, <seconds>)` — the value the
    /// samples' least-squares line reaches `seconds` past the last sample,
    /// as a float. The horizon is read at the last sample; the answer is
    /// `NONE` wherever `deriv` is.
    PredictLinear,
    /// `downsample(<value>, <instant>, <points>)` — at most `points` of the
    /// samples, chosen by the largest-triangle rule so the shape survives:
    /// the first and last always, and between them the points a line through
    /// their neighbours would lose most. The answer is an array of
    /// `{ at, value }` in time order; a group with no more samples than
    /// `points` answers all of them.
    Downsample,
    /// `decayed_sum(<value>, <instant>, <half_life>)` — the sum where each
    /// sample weighs `0.5` to the power of its age in half-lives, measured
    /// back from the newest sample, as a float.
    DecayedSum,
    /// `decayed_mean(<value>, <instant>, <half_life>)` — the weighted mean,
    /// the weighted sum over the sum of the weights.
    DecayedMean,
    /// `decayed_max(<value>, <instant>, <half_life>)` — the largest weighted
    /// value.
    DecayedMax,
    /// `decayed_count(<value>, <instant>, <half_life>)` — the sum of the
    /// weights alone: how many samples there were, counting the old ones for
    /// less.
    DecayedCount,
    /// `top_k(<expr>, <k>)` — the `k` most common present values with how
    /// many records held each, most common first and ties in the value
    /// system's order, as an array of `{ value, count }`. Exact, so it holds
    /// every distinct value of its group like `count(DISTINCT …)` and refuses
    /// past the same ceiling.
    TopK,
    /// `collect(<expr>)` — every present value, in the order the records arrived.
    ///
    /// Over nothing, `[]` and not `NONE`, by `sum`'s rule: an answer every
    /// caller has to write `?? []` after is the wrong answer.
    Collect,
    /// `approx_distinct(<expr>)` — an estimate of how many distinct present
    /// values there are (ADR-0122 C2).
    ///
    /// The only distinct count in the language, and named for what it is: an
    /// exact one holds every value, which is what a fold over a large group
    /// cannot afford. Over nothing, zero, by `count`'s rule.
    ApproxDistinct,
    /// `approx_quantile(<expr>, <rank>)` — an estimate of the value at the
    /// rank, within a stated relative error (ADR-0122 C3).
    ///
    /// Numeric like `median`, which it estimates at any rank in space that does
    /// not grow with the group. Over nothing, `NONE`, by `mean`'s rule.
    ApproxQuantile,
    /// `count(DISTINCT <expr>)` — exactly how many distinct present values there
    /// are, by the store's own `=` (ADR-0134 D8).
    ///
    /// Exact means holding every value, so a group past
    /// [`COUNT_DISTINCT_MOST`] distinct values is refused rather than grown
    /// without bound; `approx_distinct` is the fold whose space does not grow.
    CountDistinct,
    /// `state::merge(<expr>)` — the states of one fold, merged into one state
    /// (G102 S11).
    ///
    /// The one fold whose values are themselves fold states: each must be a
    /// `fold_state` of the same fold, and the answer is a `fold_state` again —
    /// finishing it is `state::finish`'s job. A state of another fold is
    /// refused rather than mis-added.
    StateMerge,
    /// `corr(<x>, <y>)` — the Pearson correlation of the pairs, as a float.
    ///
    /// Over fewer than two pairs, or where `x` never moves, `NONE`: a
    /// correlation over one point, or over a constant, is not zero — it is
    /// unasked, by `variance`'s rule.
    Corr,
    /// `covar(<x>, <y>)` — the **sample** covariance of the pairs, as a float,
    /// dividing by `n − 1` for `variance`'s reason. The population form is
    /// sayable: `covar(x, y) * (count(x) - 1) / count(x)`.
    Covar,
    /// `regr_slope(<x>, <y>)` — the least-squares slope of `y` over `x`, as a
    /// float. `NONE` where `x` never moves: no line is named by a vertical
    /// stack of points.
    RegrSlope,
    /// `regr_intercept(<x>, <y>)` — where that line crosses `x = 0`, as a
    /// float. `NONE` wherever the slope is.
    RegrIntercept,
    /// `regr_r2(<x>, <y>)` — the coefficient of determination of that line,
    /// the square of the correlation, as a float. `NONE` wherever the slope
    /// is.
    RegrR2,
    /// `quantiles(<expr>, <ranks>)` — the values at several ranks in one
    /// pass, each within [`Self::ApproxQuantile`]'s stated relative error,
    /// as an array in the ranks' order.
    ///
    /// The ranks are one array argument — `[0.5, 0.9, 0.99]` — so one sketch
    /// answers them all; a rank outside 0 to 1, or a second argument that is
    /// not an array of numbers, is refused rather than guessed at. Over
    /// nothing, `NONE`, by `approx_quantile`'s rule.
    Quantiles,
    /// `exact_quantiles(<expr>, <ranks>)` — the values at several ranks,
    /// exactly: the value at index ⌊rank × (n − 1)⌋ of the sorted values,
    /// the definition the sketch estimates.
    ///
    /// Exact means holding the group, by `median`'s rule and under the same
    /// memory ceiling; `quantiles` is the fold whose space does not grow.
    ExactQuantiles,
    /// `skewness(<expr>)` — the adjusted Fisher–Pearson coefficient, the
    /// sample form: √(n(n−1))/(n−2) · m₃/m₂^1.5, as a float. Symmetric data
    /// answers near zero. `NONE` under three values, where the sample form
    /// divides by zero — by `variance`'s rule that an answer the data cannot
    /// give is not zero.
    Skewness,
    /// `kurtosis(<expr>)` — the sample **excess** kurtosis (Fisher's): a
    /// normal shape answers near zero, a heavy-tailed one above it, a flat
    /// one below. `NONE` under four values, by `skewness`'s rule.
    Kurtosis,
    /// `entropy(<expr>)` — the Shannon entropy of the group's values in
    /// bits: how much a value drawn from the group says, `−Σ p·log₂ p` over
    /// the distinct values' frequencies. One value throughout answers zero.
    /// Holds every distinct value like `count(DISTINCT …)` and refuses past
    /// the same ceiling. Over nothing, `NONE`.
    Entropy,
    /// `gini(<expr>)` — the Gini coefficient of the group's numbers: zero is
    /// perfect equality, one is one value holding everything. Defined over
    /// numbers of none or more — a negative is refused rather than folded
    /// into a number that would mean nothing — and `NONE` over nothing or
    /// when every value is zero, where equality has no denominator.
    Gini,
    /// `histogram(<expr>, <bins>)` — the group's numbers in `<bins>`
    /// equal-width bins from the smallest to the largest, as an array of
    /// `{ from, to, count }` with the last bin holding its top edge. Over
    /// nothing, `NONE` — no bins can be named without an extent.
    Histogram,
    /// `changes(<value>, <instant>)` — how many times the samples, ordered by
    /// the instant, differ from the sample before them.
    ///
    /// A window of one sample saw no changes, and zero is the complete truth
    /// there — by `resets`' rule, unlike a rise, which is genuinely unknown.
    Changes,
    /// `irate(<value>, <instant>)` — the rise between the last two samples
    /// over the seconds between them, a fall counting from zero as
    /// `increase` counts it: the rate at the window's edge rather than across
    /// it. `NONE` under two samples, or when the two share an instant.
    Irate,
    /// `idelta(<value>, <instant>)` — the last sample minus the one before
    /// it, with no reset handling: `delta` read at the window's edge. `NONE`
    /// under two samples.
    Idelta,
    /// `ts_of_first(<value>, <instant>)` — the instant of the earliest
    /// sample. One sample answers its own instant.
    TsOfFirst,
    /// `ts_of_last(<value>, <instant>)` — the instant of the latest sample.
    TsOfLast,
    /// `ts_of_max(<value>, <instant>)` — the instant of the largest sample;
    /// of equals, the earliest.
    TsOfMax,
    /// `ts_of_min(<value>, <instant>)` — the instant of the smallest sample;
    /// of equals, the earliest.
    TsOfMin,
    /// `mad(<expr>)` — the median absolute deviation: the median of the
    /// distances from the median, exactly as `median` answers, and for the
    /// same reason — no root is taken, so no float is needed.
    ///
    /// Over nothing, `NONE` by `mean`'s rule; one value answers zero, which
    /// is the complete truth: a group of one is exactly its own middle.
    Mad,
    /// `moving_sum(<value>, <instant>, <k>)` — the sum of the `k` most
    /// recent samples in instant order: the moving total at the end of the
    /// window. Fewer than `k` samples answer over all of them — a window
    /// shorter than `k` is the whole window. Exact by `sum`'s rule, integer
    /// when every sample is.
    MovingSum,
    /// `moving_mean(<value>, <instant>, <k>)` — the mean of the `k` most
    /// recent samples, by `moving_sum`'s rule and `mean`'s: the total over
    /// how many samples there actually were. Over nothing, `NONE`.
    MovingMean,
    /// `string_agg(<expr>, <sep>)` — the group's present strings joined by
    /// `<sep>`, in the order the records arrived, as `collect` holds them.
    ///
    /// A present value that is not a string is refused rather than rendered;
    /// the separator settles once and one that differs between records is
    /// refused, by `top_k`'s rule. Over nothing, `""` — the empty join, by
    /// `collect`'s rule that an answer every caller must patch is the wrong
    /// answer.
    StringAgg,
    /// `window_funnel(<instant>, <window>, <conditions>)` — the longest
    /// prefix of the conditions a chain of the group's events meets in order,
    /// the whole chain inside `<window>` seconds of its first event. The
    /// conditions are one array argument, each a boolean over the record;
    /// the answer is a whole number from 0 to the conditions' count.
    WindowFunnel,
    /// `sequence_match(<instant>, <pattern>, <conditions>)` — whether the
    /// group holds a chain of events meeting the conditions in the pattern's
    /// order, each gap between neighbours inside the pattern's bounds: `1`
    /// or `0`. The pattern is text like `'(1)(<1800)(2)'` — condition 1,
    /// then condition 2 no more than 1800 seconds later; `(>n)` asks for at
    /// least `n` seconds. A chain with gaps is not writable as nested calls,
    /// which is why the pattern is a mini-language and not more arguments.
    SequenceMatch,
    /// `sequence_count(<instant>, <pattern>, <conditions>)` — how many
    /// non-overlapping chains the group holds, found earliest first.
    SequenceCount,
    /// `retention(<conditions>)` — per group, one `1` or `0` per condition:
    /// whether the anchor (the first condition) was met at all, and for each
    /// later condition whether it was met by a group the anchor was met in.
    /// The answer is an array of integers the conditions' length.
    Retention,
    /// `sum_by(<value>, <key>)` — the sum of the values under each key, as
    /// an object mapping every key the group wrote to its own `sum`. A key
    /// is a string, refused otherwise; a record with no value writes no key.
    SumBy,
    /// `min_by(<value>, <key>)` — the smallest value under each key, as an
    /// object, in the value system's order — `min`'s reading per key.
    MinBy,
    /// `max_by(<value>, <key>)` — the largest, on [`Self::MinBy`]'s terms.
    MaxBy,
}

/// The most distinct values `count(DISTINCT …)` holds for one group.
pub const COUNT_DISTINCT_MOST: usize = 1_000_000;

/// How much a fold holds while its group is still arriving.
///
/// The question exists because two answers to it are not interchangeable, and
/// the difference is invisible in the fold's *signature*: every fold takes many
/// values and answers one. What separates them is whether the one answer can be
/// computed as the values go past.
///
/// It is a property of the fold and not a rule about aggregation, for the same
/// reason [`crate::Purity`] is a property of the function: a single rule would
/// get one of them wrong in silence. `count`, `sum`, `mean`, `min`, `max`,
/// `variance` and `stddev` all reduce one value at a time — a spread carries a
/// count and two exact totals, bounded however long the group is.
/// `collect` and `median` cannot: `collect`'s answer **is** the collection, and
/// an exact median has to see every value before it knows which one is the
/// middle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// The answer is reducible one value at a time, in space that does not grow.
    Constant,
    /// The answer is a function of the whole group, so the whole group is held.
    WholeGroup,
}

impl Aggregate {
    /// Every fold, so a listing cannot drift from the set.
    pub const ALL: &'static [Self] = &[
        Self::Count,
        Self::Sum,
        Self::Mean,
        Self::Min,
        Self::Max,
        Self::Variance,
        Self::Stddev,
        Self::Median,
        Self::Increase,
        Self::Rate,
        Self::Delta,
        Self::Resets,
        Self::Deriv,
        Self::PredictLinear,
        Self::Downsample,
        Self::DecayedSum,
        Self::DecayedMean,
        Self::DecayedMax,
        Self::DecayedCount,
        Self::TopK,
        Self::Collect,
        Self::ApproxDistinct,
        Self::ApproxQuantile,
        Self::CountDistinct,
        Self::StateMerge,
        Self::Corr,
        Self::Covar,
        Self::RegrSlope,
        Self::RegrIntercept,
        Self::RegrR2,
        Self::Quantiles,
        Self::ExactQuantiles,
        Self::Skewness,
        Self::Kurtosis,
        Self::Entropy,
        Self::Gini,
        Self::Histogram,
        Self::Changes,
        Self::Irate,
        Self::Idelta,
        Self::TsOfFirst,
        Self::TsOfLast,
        Self::TsOfMax,
        Self::TsOfMin,
        Self::Mad,
        Self::MovingSum,
        Self::MovingMean,
        Self::StringAgg,
        Self::WindowFunnel,
        Self::SequenceMatch,
        Self::SequenceCount,
        Self::Retention,
        Self::SumBy,
        Self::MinBy,
        Self::MaxBy,
    ];

    /// How the fold is written.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Mean => "mean",
            Self::Min => "min",
            Self::Max => "max",
            Self::Variance => "variance",
            Self::Stddev => "stddev",
            Self::Median => "median",
            Self::Increase => "increase",
            Self::Rate => "rate",
            Self::Delta => "delta",
            Self::Resets => "resets",
            Self::Deriv => "deriv",
            Self::PredictLinear => "predict_linear",
            Self::Downsample => "downsample",
            Self::DecayedSum => "decayed_sum",
            Self::DecayedMean => "decayed_mean",
            Self::DecayedMax => "decayed_max",
            Self::DecayedCount => "decayed_count",
            Self::TopK => "top_k",
            Self::Collect => "collect",
            Self::ApproxDistinct => "approx_distinct",
            Self::ApproxQuantile => "approx_quantile",
            // Written `count(DISTINCT x)`; this spelling names it in messages and
            // cannot be read as a word, so `count` stays the plain count.
            Self::CountDistinct => "count distinct",
            // Namespaced like a function; the parser recognises the three
            // tokens rather than one word.
            Self::StateMerge => "state::merge",
            Self::Corr => "corr",
            Self::Covar => "covar",
            Self::RegrSlope => "regr_slope",
            Self::RegrIntercept => "regr_intercept",
            Self::RegrR2 => "regr_r2",
            Self::Quantiles => "quantiles",
            Self::ExactQuantiles => "exact_quantiles",
            Self::Skewness => "skewness",
            Self::Kurtosis => "kurtosis",
            Self::Entropy => "entropy",
            Self::Gini => "gini",
            Self::Histogram => "histogram",
            Self::Changes => "changes",
            Self::Irate => "irate",
            Self::Idelta => "idelta",
            Self::TsOfFirst => "ts_of_first",
            Self::TsOfLast => "ts_of_last",
            Self::TsOfMax => "ts_of_max",
            Self::TsOfMin => "ts_of_min",
            Self::Mad => "mad",
            Self::MovingSum => "moving_sum",
            Self::MovingMean => "moving_mean",
            Self::StringAgg => "string_agg",
            Self::WindowFunnel => "window_funnel",
            Self::SequenceMatch => "sequence_match",
            Self::SequenceCount => "sequence_count",
            Self::Retention => "retention",
            Self::SumBy => "sum_by",
            Self::MinBy => "min_by",
            Self::MaxBy => "max_by",
        }
    }

    /// What this fold holds while its group arrives.
    ///
    /// Read by the executor's memory ceiling, which used to exempt every folding
    /// read by name on the grounds that *"its answer does not grow with the
    /// table"*. That was true of every fold the language had; it is false of
    /// `collect`, whose answer is the table (Q-227).
    #[must_use]
    pub const fn retention(self) -> Retention {
        match self {
            Self::Count
            | Self::Sum
            | Self::Mean
            | Self::Min
            | Self::Max
            | Self::Variance
            | Self::Stddev
            | Self::ApproxDistinct
            | Self::ApproxQuantile => Retention::Constant,
            Self::StateMerge => Retention::Constant,
            Self::Corr | Self::Covar | Self::RegrSlope | Self::RegrIntercept | Self::RegrR2 => {
                Retention::Constant
            }
            Self::Quantiles | Self::Skewness | Self::Kurtosis => Retention::Constant,
            Self::ExactQuantiles | Self::Entropy | Self::Gini | Self::Histogram => {
                Retention::WholeGroup
            }
            Self::Median
            | Self::Increase
            | Self::Rate
            | Self::Delta
            | Self::Resets
            | Self::Deriv
            | Self::PredictLinear
            | Self::Downsample
            | Self::DecayedSum
            | Self::DecayedMean
            | Self::DecayedMax
            | Self::DecayedCount
            | Self::TopK
            | Self::Collect
            | Self::CountDistinct
            | Self::Changes
            | Self::Irate
            | Self::Idelta
            | Self::TsOfFirst
            | Self::TsOfLast
            | Self::TsOfMax
            | Self::TsOfMin
            | Self::Mad
            | Self::MovingSum
            | Self::MovingMean
            | Self::StringAgg
            | Self::WindowFunnel
            | Self::SequenceMatch
            | Self::SequenceCount
            | Self::Retention
            | Self::SumBy
            | Self::MinBy
            | Self::MaxBy => Retention::WholeGroup,
        }
    }

    /// Whether the fold orders its values by an instant, and so takes one.
    #[must_use]
    pub const fn takes_an_instant(self) -> bool {
        matches!(
            self,
            Self::Increase
                | Self::Rate
                | Self::Delta
                | Self::Resets
                | Self::Deriv
                | Self::PredictLinear
                | Self::Downsample
                | Self::DecayedSum
                | Self::DecayedMean
                | Self::DecayedMax
                | Self::DecayedCount
                | Self::Changes
                | Self::Irate
                | Self::Idelta
                | Self::TsOfFirst
                | Self::TsOfLast
                | Self::TsOfMax
                | Self::TsOfMin
                | Self::MovingSum
                | Self::MovingMean
        )
    }

    /// What the fold's third argument is, for a fold that takes one — the
    /// horizon a projection or a decay is asked at, or the conditions a
    /// sequence fold reads; `None` for a fold with no third argument.
    #[must_use]
    pub const fn third(self) -> Option<&'static str> {
        match self {
            Self::PredictLinear
            | Self::Downsample
            | Self::DecayedSum
            | Self::DecayedMean
            | Self::DecayedMax
            | Self::DecayedCount
            | Self::MovingSum
            | Self::MovingMean => Some("`,` and the horizon in seconds"),
            Self::WindowFunnel | Self::SequenceMatch | Self::SequenceCount => {
                Some("`,` and the conditions, an array of booleans")
            }
            _ => None,
        }
    }

    /// Whether the fold takes a third argument — [`Self::third`] naming it.
    #[must_use]
    pub const fn takes_ahead(self) -> bool {
        self.third().is_some()
    }

    /// What the fold's second argument is, for a fold that takes one: the
    /// instant a counter fold orders by, or the rank a quantile is asked at.
    #[must_use]
    pub const fn second(self) -> Option<&'static str> {
        match self {
            Self::Increase
            | Self::Rate
            | Self::Delta
            | Self::Resets
            | Self::Deriv
            | Self::PredictLinear
            | Self::Downsample
            | Self::DecayedSum
            | Self::DecayedMean
            | Self::DecayedMax
            | Self::DecayedCount
            | Self::Changes
            | Self::Irate
            | Self::Idelta
            | Self::TsOfFirst
            | Self::TsOfLast
            | Self::TsOfMax
            | Self::TsOfMin
            | Self::MovingSum
            | Self::MovingMean => Some("`,` and the instant each value was observed at"),
            Self::ApproxQuantile => Some("`,` and the rank, a number from 0 to 1"),
            Self::TopK => Some("`,` and how many of the most common values to answer"),
            Self::Corr | Self::Covar | Self::RegrSlope | Self::RegrIntercept | Self::RegrR2 => {
                Some("`,` and the value paired with it")
            }
            Self::Quantiles | Self::ExactQuantiles => {
                Some("`,` and the ranks, an array of numbers from 0 to 1")
            }
            Self::Histogram => Some("`,` and how many bins to answer"),
            Self::StringAgg => Some("`,` and the separator to join the values with"),
            Self::WindowFunnel => Some("`,` and the window in seconds"),
            Self::SumBy | Self::MinBy | Self::MaxBy => {
                Some("`,` and the key each value is folded under")
            }
            Self::SequenceMatch | Self::SequenceCount => {
                Some("`,` and the pattern, like '(1)(<1800)(2)'")
            }
            _ => None,
        }
    }

    /// Whether the fold's answer is an estimate rather than exact.
    #[must_use]
    pub const fn estimates(self) -> bool {
        matches!(
            self,
            Self::ApproxDistinct | Self::ApproxQuantile | Self::Quantiles
        )
    }

    /// The fold a word spells, if it spells one.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|fold| fold.spelling().eq_ignore_ascii_case(word))
    }
}
