//! The comparison rule, one bound at a time.

#![allow(clippy::panic)]

use core::time::Duration;

use super::{Baseline, Conditions, DECLARED, Phase, Spread, compare, folded, parse, render};
use crate::samples::Report;

fn conditions() -> Conditions {
    Conditions {
        machine: "macos/aarch64".to_owned(),
        build: "release".to_owned(),
        backend: "memory".to_owned(),
        runs: 5,
    }
}

fn phase(p50: (u64, u64, u64), p99: (u64, u64, u64)) -> Phase {
    Phase {
        workload: "write".to_owned(),
        phase: "insert".to_owned(),
        p50: Spread {
            median: p50.0,
            low: p50.1,
            high: p50.2,
        },
        p99: Spread {
            median: p99.0,
            low: p99.1,
            high: p99.2,
        },
    }
}

fn of(phases: Vec<Phase>) -> Baseline {
    Baseline {
        conditions: conditions(),
        thresholds: DECLARED,
        label: "test".to_owned(),
        phases,
    }
}

const STEADY: (u64, u64, u64) = (100_000, 98_000, 104_000);

#[test]
fn a_phase_past_the_threshold_and_every_run_breaches() {
    let was = of(vec![phase(STEADY, STEADY)]);
    let now = of(vec![phase((115_000, 115_000, 115_000), STEADY)]);
    let found = compare(&was, &now);
    assert_eq!(found.compared, 1);
    assert_eq!(found.breaches.len(), 1, "{found:?}");
    assert_eq!(
        found.breaches.first().map(|held| held.percentile),
        Some("p50")
    );
}

#[test]
fn a_move_inside_the_threshold_does_not_breach() {
    let was = of(vec![phase(STEADY, STEADY)]);
    let now = of(vec![phase(
        (109_000, 109_000, 109_000),
        (149_000, 149_000, 149_000),
    )]);
    assert!(compare(&was, &now).breaches.is_empty());
}

#[test]
fn a_move_inside_the_runs_the_baseline_saw_does_not_breach() {
    // The baseline's own runs reached 130 µs once; a median there is noise
    // the baseline already contained, whatever the percentage says.
    let was = of(vec![phase((100_000, 98_000, 130_000), STEADY)]);
    let now = of(vec![phase((125_000, 125_000, 125_000), STEADY)]);
    assert!(compare(&was, &now).breaches.is_empty());
}

#[test]
fn one_slow_run_is_not_a_slower_build() {
    // The median landed on noisy runs; the fastest run is where it was.
    let was = of(vec![phase(STEADY, STEADY)]);
    let now = of(vec![phase((140_000, 101_000, 150_000), STEADY)]);
    assert!(compare(&was, &now).breaches.is_empty());
}

#[test]
fn a_move_below_the_timer_floor_does_not_breach() {
    let was = of(vec![phase((500, 500, 500), STEADY)]);
    let now = of(vec![phase((900, 900, 900), STEADY)]);
    assert!(compare(&was, &now).breaches.is_empty());
}

#[test]
fn the_tail_has_its_own_threshold() {
    let was = of(vec![phase(STEADY, STEADY)]);
    let now = of(vec![phase(STEADY, (155_000, 155_000, 155_000))]);
    let found = compare(&was, &now);
    assert_eq!(
        found.breaches.first().map(|held| held.percentile),
        Some("p99")
    );
}

#[test]
fn other_conditions_could_not_be_checked_and_are_not_a_pass() {
    let was = of(vec![phase(STEADY, STEADY)]);
    let mut now = of(vec![phase(STEADY, STEADY)]);
    now.conditions.machine = "linux/x86_64".to_owned();
    let found = compare(&was, &now);
    assert_eq!(found.compared, 0);
    assert_eq!(found.unchecked.len(), 1);
}

#[test]
fn a_phase_on_one_side_only_is_named() {
    let was = of(vec![phase(STEADY, STEADY)]);
    let mut renamed = phase(STEADY, STEADY);
    renamed.phase = "insert one".to_owned();
    let found = compare(&was, &of(vec![renamed]));
    assert_eq!(found.unchecked.len(), 2, "{found:?}");
}

#[test]
fn a_file_reads_back_as_it_was_written() {
    let written = of(vec![phase(STEADY, (200_000, 190_000, 260_000))]);
    assert_eq!(parse(&render(&written)), Ok(written));
}

#[test]
fn something_else_is_not_read_as_a_baseline() {
    assert!(parse("# TessariDB benchmark\n").is_err());
    let mut text = render(&of(vec![phase(STEADY, STEADY)]));
    text.push_str("write\tinsert\t1\t2\n");
    assert!(parse(&text).is_err());
}

fn report(name: &str, p50: u64, p99: u64) -> Report {
    Report {
        phase: name.to_owned(),
        operations: 10,
        total: Duration::from_micros(10),
        p50: Duration::from_nanos(p50),
        p90: Duration::ZERO,
        p95: Duration::ZERO,
        p99: Duration::from_nanos(p99),
        max: Duration::ZERO,
        note: None,
    }
}

#[test]
fn runs_fold_into_the_median_and_the_range_and_notes_are_left_out() {
    let runs: Vec<Vec<Report>> = [300, 100, 200]
        .into_iter()
        .map(|p50| {
            vec![
                report("insert", p50, p50 * 2),
                Report::measurement("bytes", "12 MB"),
            ]
        })
        .collect();
    let phases = folded("write", &runs);
    assert_eq!(phases.len(), 1);
    let only = phases.first().map(|held| (held.p50, held.p99));
    assert_eq!(
        only,
        Some((
            Spread {
                median: 200,
                low: 100,
                high: 300
            },
            Spread {
                median: 400,
                low: 200,
                high: 600
            }
        ))
    );
}
