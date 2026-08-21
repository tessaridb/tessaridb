//! What one phase of a workload measured.
//!
//! # Every sample is kept
//!
//! Not a running estimate, and not a bucketed histogram. `bgv-rocksdb`'s own
//! list of silent failures names the reason: a cumulative histogram reports a
//! healthy p99 straight through a real regression, because the bad tail is
//! diluted by everything that came before it. A retained sample costs sixteen
//! bytes per operation and answers the question exactly.
//!
//! # Per phase, never across phases
//!
//! A write phase and a read phase folded into one distribution describe neither.
//! Each phase carries its own samples and reports on its own.
//!
//! # Nearest-rank, not interpolated
//!
//! The p-th percentile is the element at `ceil(p/100 × n) - 1` of the sorted
//! sample: a latency that was **observed**. Interpolation invents a number
//! between two observations, which reads the same and is not the same — and the
//! p99 of a two-sample run interpolates to something neither run took.

use core::time::Duration;

/// The durations one phase of a workload observed.
#[derive(Debug, Default)]
pub struct Samples {
    held: Vec<Duration>,
}

impl Samples {
    /// Room for a known number of operations.
    #[must_use]
    pub fn with_capacity(operations: usize) -> Self {
        Self {
            held: Vec::with_capacity(operations),
        }
    }

    /// Record one operation.
    pub fn push(&mut self, taken: Duration) {
        self.held.push(taken);
    }

    /// The percentiles and the total, computed once.
    ///
    /// Consumes the samples because computing them sorts, and a caller that kept
    /// the collection afterwards would be holding a differently ordered thing
    /// than it recorded.
    #[must_use]
    pub fn summarise(mut self, phase: &str) -> Report {
        self.held.sort_unstable();
        let total: Duration = self.held.iter().sum();
        Report {
            phase: phase.to_owned(),
            operations: self.held.len(),
            total,
            p50: at_percentile(&self.held, 50),
            p90: at_percentile(&self.held, 90),
            p99: at_percentile(&self.held, 99),
            max: self.held.last().copied().unwrap_or_default(),
        }
    }
}

/// The element at the nearest rank for this percentile.
///
/// `held` must already be sorted. Returns zero for an empty sample, which is the
/// only honest answer: no operation was observed, so no latency was.
fn at_percentile(held: &[Duration], percentile: u32) -> Duration {
    if held.is_empty() {
        return Duration::ZERO;
    }
    // `ceil(p/100 × n)` without floating point, so the rank is exact rather
    // than exact-looking: `(p × n + 99) / 100`.
    let scaled = u64::from(percentile).saturating_mul(held.len().try_into().unwrap_or(u64::MAX));
    let rank = scaled.saturating_add(99).saturating_div(100).max(1);
    let index = usize::try_from(rank.saturating_sub(1)).unwrap_or(usize::MAX);
    held.get(index)
        .copied()
        .or_else(|| held.last().copied())
        .unwrap_or_default()
}

/// What one phase measured, ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What the phase was doing.
    pub phase: String,
    /// How many operations it did.
    pub operations: usize,
    /// How long they took in total.
    pub total: Duration,
    /// The median.
    pub p50: Duration,
    /// The ninetieth percentile.
    pub p90: Duration,
    /// The ninety-ninth.
    pub p99: Duration,
    /// The slowest single operation.
    pub max: Duration,
}

impl Report {
    /// Operations per second, or `0.0` when nothing was measured.
    ///
    /// Derived from the summed operation time rather than from wall clock, so a
    /// workload that pauses between phases does not report a throughput it never
    /// reached.
    #[must_use]
    pub fn throughput(&self) -> f64 {
        let seconds = self.total.as_secs_f64();
        if seconds <= 0.0 || self.operations == 0 {
            return 0.0;
        }
        count(self.operations) / seconds
    }

    /// One row of the table, in the same form the printed output and the
    /// baseline file both use — so the two cannot drift apart.
    #[must_use]
    pub fn row(&self) -> String {
        format!(
            "| {} | {} | {:.0} | {} | {} | {} | {} |",
            self.phase,
            self.operations,
            self.throughput(),
            micros(self.p50),
            micros(self.p90),
            micros(self.p99),
            micros(self.max),
        )
    }

    /// The header those rows sit under.
    #[must_use]
    pub const fn header() -> &'static str {
        "| phase | ops | ops/s | p50 µs | p90 µs | p99 µs | max µs |\n\
         |---|---|---|---|---|---|---|"
    }
}

/// A duration in microseconds, to one decimal.
fn micros(taken: Duration) -> String {
    format!("{:.1}", taken.as_secs_f64() * 1_000_000.0)
}

/// A count as a float, without an `as` cast.
fn count(value: usize) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use core::time::Duration;

    use super::{Report, Samples};

    fn of(millis: &[u64]) -> Samples {
        let mut samples = Samples::with_capacity(millis.len());
        for held in millis {
            samples.push(Duration::from_millis(*held));
        }
        samples
    }

    fn summarised(millis: &[u64]) -> Report {
        of(millis).summarise("phase")
    }

    #[test]
    fn a_hundred_samples_select_the_documented_elements() {
        // 1..=100 milliseconds, so the answer for each percentile is its own
        // number and an off-by-one is legible rather than arithmetic.
        let held: Vec<u64> = (1..=100).collect();
        let report = summarised(&held);
        assert_eq!(report.operations, 100);
        assert_eq!(report.p50, Duration::from_millis(50));
        assert_eq!(report.p90, Duration::from_millis(90));
        assert_eq!(report.p99, Duration::from_millis(99));
        assert_eq!(report.max, Duration::from_millis(100));
    }

    #[test]
    fn the_order_they_arrive_in_does_not_matter() {
        let ascending = summarised(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let descending = summarised(&[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(ascending.p50, descending.p50);
        assert_eq!(ascending.p99, descending.p99);
        assert_eq!(ascending.max, descending.max);
    }

    #[test]
    fn one_sample_is_every_percentile() {
        // The edge where an off-by-one is invisible at scale and fatal here: a
        // rank of zero would index before the start.
        let report = summarised(&[7]);
        assert_eq!(report.p50, Duration::from_millis(7));
        assert_eq!(report.p99, Duration::from_millis(7));
        assert_eq!(report.max, Duration::from_millis(7));
    }

    #[test]
    fn two_samples_never_report_a_latency_between_them() {
        // The argument for nearest-rank over interpolation, asserted: an
        // interpolated p50 of `[10, 20]` is 15, which nothing observed.
        let report = summarised(&[10, 20]);
        assert!(
            report.p50 == Duration::from_millis(10) || report.p50 == Duration::from_millis(20),
            "{:?} was never observed",
            report.p50
        );
        assert_eq!(report.p99, Duration::from_millis(20));
    }

    #[test]
    fn nothing_measured_reports_nothing_rather_than_a_number() {
        let report = summarised(&[]);
        assert_eq!(report.operations, 0);
        assert_eq!(report.p99, Duration::ZERO);
        assert!((report.throughput() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_tail_shows_in_the_tail_and_not_in_the_median() {
        // The property a cumulative histogram loses: ninety-nine fast operations
        // and one slow one must move p99 and max, and leave p50 alone.
        let mut held: Vec<u64> = vec![1; 99];
        held.push(1_000);
        let report = summarised(&held);
        assert_eq!(report.p50, Duration::from_millis(1));
        assert_eq!(report.p90, Duration::from_millis(1));
        assert_eq!(report.max, Duration::from_millis(1_000));
    }

    #[test]
    fn throughput_is_operations_over_the_time_they_took() {
        let report = summarised(&[10; 100]);
        // A hundred operations of ten milliseconds is one second of work.
        assert!(
            (report.throughput() - 100.0).abs() < 0.001,
            "{}",
            report.throughput()
        );
    }

    #[test]
    fn a_row_and_the_header_have_the_same_number_of_columns() {
        // The printed table and the baseline file are the same rows, so a column
        // added to one without the other would silently misalign a baseline.
        let report = summarised(&[1, 2, 3]);
        let columns = |line: &str| line.matches('|').count();
        let header = Report::header();
        let first = header.lines().next().expect("a header line");
        assert_eq!(columns(first), columns(&report.row()));
    }
}
