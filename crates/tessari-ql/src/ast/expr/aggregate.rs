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
    /// `collect(<expr>)` — every present value, in the order the records arrived.
    ///
    /// Over nothing, `[]` and not `NONE`, by `sum`'s rule: an answer every
    /// caller has to write `?? []` after is the wrong answer.
    Collect,
}

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
        Self::Collect,
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
            Self::Collect => "collect",
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
            | Self::Stddev => Retention::Constant,
            Self::Median | Self::Increase | Self::Rate | Self::Delta | Self::Collect => {
                Retention::WholeGroup
            }
        }
    }

    /// Whether the fold orders its values by an instant, and so takes one.
    #[must_use]
    pub const fn takes_an_instant(self) -> bool {
        matches!(self, Self::Increase | Self::Rate | Self::Delta)
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
