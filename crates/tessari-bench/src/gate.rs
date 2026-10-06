//! Recording a baseline and comparing a release with it.
//!
//! The workloads here are the release set: one per engine path a release can
//! slow down without failing a test — a write, the three read paths, the text
//! index, paging, an update that moves an index, and the vector scan. The
//! workloads that measure a ratio (vector recall, memory, capacity) or depend on
//! the disk are left out, because their spread on one machine is wider than the
//! regressions a release gate is for.

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use crate::baseline::{self, Baseline, Conditions, DECLARED};
use crate::samples::Report;
use crate::workload::{self, Workload};

/// The workloads a release re-measures.
pub const RELEASE: &[&str] = &[
    "write",
    "read-by-id",
    "filter",
    "range",
    "search",
    "paging",
    "update",
    "vector",
];

/// Run every workload of the release set `runs` times, each on a fresh store.
pub fn measured(
    backend: &str,
    runs: u32,
    label: &str,
    measure: impl Fn(&Workload) -> Result<Vec<Report>, String>,
) -> Result<Baseline, String> {
    let mut phases = Vec::new();
    for name in RELEASE {
        let held = workload::by_name(name).ok_or_else(|| format!("no workload {name:?}"))?;
        let mut taken = Vec::new();
        for run in 1..=runs {
            eprintln!("{name}: run {run} of {runs}");
            taken.push(measure(held)?);
        }
        phases.extend(baseline::folded(name, &taken));
    }
    Ok(Baseline {
        conditions: Conditions {
            machine: format!("{}/{}", env::consts::OS, env::consts::ARCH),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            backend: backend.to_owned(),
            runs,
        },
        thresholds: DECLARED,
        label: label.to_owned(),
        phases,
    })
}

/// Write a measurement as the baseline.
pub fn record(path: &Path, measured: &Baseline) -> Result<ExitCode, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|failure| failure.to_string())?;
    }
    fs::write(path, baseline::render(measured)).map_err(|failure| failure.to_string())?;
    println!(
        "baseline: {} phases over {} runs written to {}",
        measured.phases.len(),
        measured.conditions.runs,
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Compare a measurement with the baseline at `path`.
///
/// Exit 0 when every phase was compared and none breached, 1 on a breach, and 2
/// when something could not be checked — a different machine, build or backend,
/// or a phase on one side only. A comparison that could not be made is never
/// reported as one that passed.
pub fn compare(path: &Path, now: &Baseline) -> Result<ExitCode, String> {
    let text =
        fs::read_to_string(path).map_err(|failure| format!("{}: {failure}", path.display()))?;
    let was = baseline::parse(&text)?;
    let found = baseline::compare(&was, now);
    println!("baseline: {} ({})", path.display(), was.label);
    println!(
        "thresholds: p50 {} %, p99 {} %, floor {} ns, outside the baseline's {} runs",
        was.thresholds.p50_percent,
        was.thresholds.p99_percent,
        was.thresholds.floor_nanos,
        was.conditions.runs
    );
    for unchecked in &found.unchecked {
        println!("NOT CHECKED  {unchecked}");
    }
    for breach in &found.breaches {
        println!(
            "BREACH  {} / {} {}: {} ns -> {} ns",
            breach.workload, breach.phase, breach.percentile, breach.was, breach.now
        );
    }
    println!(
        "compared {} phases: {} breached, {} not checked",
        found.compared,
        found.breaches.len(),
        found.unchecked.len()
    );
    Ok(if !found.unchecked.is_empty() {
        ExitCode::from(2)
    } else if found.breaches.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
