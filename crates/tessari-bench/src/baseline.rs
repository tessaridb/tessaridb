//! A recorded baseline, and the comparison a release makes against it.
//!
//! # Why a release can gate on this when a CI runner could not
//!
//! The harness's numbers are comparable on one machine with one build profile and
//! nothing else. A shared runner breaks that; the release step does not, because
//! it runs on the machine the baseline names and refuses to compare otherwise.
//! Noise within that machine is handled by the rule, not by a person squinting:
//! every workload runs several times on a fresh store, the baseline keeps the
//! median AND the range its runs covered, and a phase breaches only when it is
//! past the declared threshold, past every run the baseline saw, and past the
//! timer floor.
//!
//! The thresholds are written into the file when it is recorded, so they are
//! declared before any comparison is measured against them, and moving them is a
//! change somebody can see.

use core::time::Duration;

use crate::samples::Report;

/// The file's first line; a file without it is not one of these.
const MAGIC: &str = "# tessari-bench baseline v1";

/// How far a phase's median may move before it counts, in percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub p50_percent: u64,
    pub p99_percent: u64,
    /// Below this absolute move nothing counts: the clock's own granularity.
    pub floor_nanos: u64,
}

/// The defaults a new baseline declares: p50 10 %, p99 50 %, 1 µs.
///
/// The p99 figure is measured, not a preference. On the release machine the
/// same build compared with its own baseline moved a microsecond write's p99 by
/// up to 42 % between sessions while every p50 held: a tail that short is the
/// operating system's scheduler as much as the engine's, so a 20 % gate there
/// fails an unchanged build and stops being believed.
pub const DECLARED: Thresholds = Thresholds {
    p50_percent: 10,
    p99_percent: 50,
    floor_nanos: 1_000,
};

/// One percentile of one phase across every run: the median and the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spread {
    pub median: u64,
    pub low: u64,
    pub high: u64,
}

/// One phase of one workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub workload: String,
    pub phase: String,
    pub p50: Spread,
    pub p99: Spread,
}

/// What a set of measurements is comparable with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conditions {
    pub machine: String,
    pub build: String,
    pub backend: String,
    pub runs: u32,
}

/// A recorded baseline, or the measurement a release compares with one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub conditions: Conditions,
    pub thresholds: Thresholds,
    pub label: String,
    pub phases: Vec<Phase>,
}

/// One percentile of one phase that moved past every bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breach {
    pub workload: String,
    pub phase: String,
    pub percentile: &'static str,
    pub was: u64,
    pub now: u64,
}

/// What a comparison found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    pub breaches: Vec<Breach>,
    /// Why the two cannot be compared at all, or which phases one side lacks.
    /// Either is "could not check", never a pass.
    pub unchecked: Vec<String>,
    pub compared: usize,
}

/// The phases of several runs of one workload, folded into one spread each.
///
/// A row that carries a note instead of timings says nothing to compare, so it
/// is left out. Phases are matched by name; a phase missing from some run is
/// folded over the runs that have it.
pub fn folded(workload: &str, runs: &[Vec<Report>]) -> Vec<Phase> {
    let mut names: Vec<&str> = Vec::new();
    for report in runs.iter().flatten() {
        if report.note.is_none() && !names.contains(&report.phase.as_str()) {
            names.push(&report.phase);
        }
    }
    names
        .into_iter()
        .map(|name| {
            let taken = |pick: fn(&Report) -> Duration| -> Spread {
                spread(
                    runs.iter()
                        .flatten()
                        .filter(|report| report.note.is_none() && report.phase == name)
                        .map(|report| nanos(pick(report)))
                        .collect(),
                )
            };
            Phase {
                workload: workload.to_owned(),
                phase: name.to_owned(),
                p50: taken(|report| report.p50),
                p99: taken(|report| report.p99),
            }
        })
        .collect()
}

/// The lower median and the range of some values.
fn spread(mut values: Vec<u64>) -> Spread {
    values.sort_unstable();
    let middle = values.len().saturating_sub(1) / 2;
    Spread {
        median: values.get(middle).copied().unwrap_or(0),
        low: values.first().copied().unwrap_or(0),
        high: values.last().copied().unwrap_or(0),
    }
}

fn nanos(taken: Duration) -> u64 {
    u64::try_from(taken.as_nanos()).unwrap_or(u64::MAX)
}

/// Compare a measurement with a baseline, by the baseline's own thresholds.
pub fn compare(baseline: &Baseline, now: &Baseline) -> Comparison {
    let mut found = Comparison::default();
    if baseline.conditions != now.conditions {
        found.unchecked.push(format!(
            "recorded under {:?}, measured under {:?}",
            baseline.conditions, now.conditions
        ));
        return found;
    }
    let limits = baseline.thresholds;
    for was in &baseline.phases {
        let Some(current) = now
            .phases
            .iter()
            .find(|held| held.workload == was.workload && held.phase == was.phase)
        else {
            found.unchecked.push(format!(
                "{} / {}: not measured now",
                was.workload, was.phase
            ));
            continue;
        };
        found.compared = found.compared.saturating_add(1);
        for (percentile, before, after, percent) in [
            ("p50", was.p50, current.p50, limits.p50_percent),
            ("p99", was.p99, current.p99, limits.p99_percent),
        ] {
            if breached(before, after.low, percent, limits.floor_nanos) {
                found.breaches.push(Breach {
                    workload: was.workload.clone(),
                    phase: was.phase.clone(),
                    percentile,
                    was: before.median,
                    now: after.low,
                });
            }
        }
    }
    for held in &now.phases {
        let known = baseline
            .phases
            .iter()
            .any(|was| was.workload == held.workload && was.phase == held.phase);
        if !known {
            found.unchecked.push(format!(
                "{} / {}: not in the baseline",
                held.workload, held.phase
            ));
        }
    }
    found
}

/// Past the threshold, past every run the baseline saw, and past the floor —
/// asked of the FASTEST run measured now. A change in the code slows every run;
/// a noisy moment slows some, and the median of five can still land on one.
fn breached(before: Spread, now: u64, percent: u64, floor: u64) -> bool {
    let allowed = u128::from(before.median)
        .saturating_mul(u128::from(percent.saturating_add(100)))
        .saturating_div(100);
    u128::from(now) > allowed && now > before.high && now.saturating_sub(before.median) >= floor
}

/// The file a baseline is kept in.
pub fn render(baseline: &Baseline) -> String {
    let conditions = &baseline.conditions;
    let limits = baseline.thresholds;
    let mut out = format!(
        "{MAGIC}\n# machine\t{}\n# build\t{}\n# backend\t{}\n# runs\t{}\n\
         # threshold-p50\t{}\n# threshold-p99\t{}\n# floor-ns\t{}\n# label\t{}\n\
         workload\tphase\tp50\tp50-low\tp50-high\tp99\tp99-low\tp99-high\n",
        conditions.machine,
        conditions.build,
        conditions.backend,
        conditions.runs,
        limits.p50_percent,
        limits.p99_percent,
        limits.floor_nanos,
        baseline.label,
    );
    for phase in &baseline.phases {
        let (p50, p99) = (phase.p50, phase.p99);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            phase.workload,
            phase.phase,
            p50.median,
            p50.low,
            p50.high,
            p99.median,
            p99.low,
            p99.high
        ));
    }
    out
}

/// Read a baseline file back, refusing anything that is not one.
pub fn parse(text: &str) -> Result<Baseline, String> {
    let mut lines = text.lines();
    if lines.next() != Some(MAGIC) {
        return Err(format!("not a baseline: the first line is not {MAGIC:?}"));
    }
    let mut header = |key: &str| -> Result<String, String> {
        let line = lines
            .next()
            .ok_or_else(|| format!("the header ends before {key}"))?;
        line.strip_prefix("# ")
            .and_then(|rest| rest.strip_prefix(key))
            .and_then(|rest| rest.strip_prefix('\t'))
            .map(str::to_owned)
            .ok_or_else(|| format!("expected the header {key}, found {line:?}"))
    };
    let number = |value: String, key: &str| -> Result<u64, String> {
        value
            .parse()
            .map_err(|_| format!("{key} is not a number: {value:?}"))
    };
    let machine = header("machine")?;
    let build = header("build")?;
    let backend = header("backend")?;
    let runs = header("runs")?;
    let p50 = header("threshold-p50")?;
    let p99 = header("threshold-p99")?;
    let floor = header("floor-ns")?;
    let label = header("label")?;
    let conditions = Conditions {
        machine,
        build,
        backend,
        runs: u32::try_from(number(runs, "runs")?).map_err(|_| "runs is too large".to_owned())?,
    };
    let thresholds = Thresholds {
        p50_percent: number(p50, "threshold-p50")?,
        p99_percent: number(p99, "threshold-p99")?,
        floor_nanos: number(floor, "floor-ns")?,
    };
    lines
        .next()
        .ok_or_else(|| "the column header is missing".to_owned())?;
    let mut phases = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let cells: Vec<&str> = line.split('\t').collect();
        let [workload, phase, values @ ..] = cells.as_slice() else {
            return Err(format!("a row with too few cells: {line:?}"));
        };
        let values: Vec<u64> = values
            .iter()
            .map(|cell| {
                cell.parse()
                    .map_err(|_| format!("not a number in {line:?}"))
            })
            .collect::<Result<_, _>>()?;
        let [p50, p50_low, p50_high, p99, p99_low, p99_high] = values.as_slice() else {
            return Err(format!("a row needs six numbers: {line:?}"));
        };
        phases.push(Phase {
            workload: (*workload).to_owned(),
            phase: (*phase).to_owned(),
            p50: Spread {
                median: *p50,
                low: *p50_low,
                high: *p50_high,
            },
            p99: Spread {
                median: *p99,
                low: *p99_low,
                high: *p99_high,
            },
        });
    }
    Ok(Baseline {
        conditions,
        thresholds,
        label,
        phases,
    })
}

#[cfg(test)]
mod tests;
