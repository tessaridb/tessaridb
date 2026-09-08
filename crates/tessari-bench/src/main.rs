//! A workload harness for TessariDB.
//!
//! # What it is for
//!
//! Five things in this project are waiting on a number rather than on code: the
//! three memory-budget terms (Q-28), BM25's `k1` and `b` (Q-47), whether the
//! planner's rule needs to become a cost model, the readiness checklist's
//! capacity row, and the recall floor an HNSW index would have to clear. All
//! five are of the form *how much load does this take before it stops meeting a
//! bound* — which is a workload, a duration and a percentile, not an iteration
//! count. So this is a workload harness and not a microbenchmark runner.
//!
//! # What a number here is worth, and what it is not
//!
//! A baseline is recorded **per machine**, with the machine written into the
//! file. It is compared by a person, deliberately. It is deliberately not a CI
//! gate: a shared runner's timings vary by more than the regressions worth
//! catching, so an automatic check would either fail constantly or be loosened
//! until it never failed — and a check that never fails is worse than none,
//! because it is also believed.
//!
//! # Usage
//!
//! ```text
//! cargo run -p tessari-bench --release                     every workload, in memory
//! cargo run -p tessari-bench --release -- --list           what there is to run
//! cargo run -p tessari-bench --release -- --workload write one of them
//! cargo run -p tessari-bench --release -- --backend disk   against the on-disk engine
//! cargo run -p tessari-bench --release -- --baseline benchmarks/today.md
//! ```
//!
//! Release matters and the harness says so if it was not: a debug build measures
//! the debug build, and reporting those numbers as the store's would be a lie
//! that looks like data.

#[cfg(feature = "counting")]
mod counting;
#[cfg(feature = "counting")]
mod memory;
mod paging;
mod queue;
mod ranges;
mod samples;
mod workload;

/// A running total in front of the system allocator, in the counting build only.
///
/// Behind a feature because a global allocator is global: making it the default
/// would put an atomic add on every allocation in every workload, including the
/// ones whose recorded baselines are timings taken without it. Off, the ordinary
/// build is what it was; on, the preamble says so.
#[cfg(feature = "counting")]
#[global_allocator]
static ALLOCATOR: counting::Counting = counting::Counting;

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tessaridb::Db;

use crate::samples::Report;
use crate::workload::Workload;

/// Which engine the workloads run against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Everything in memory: measures the layers above the engine.
    Memory,
    /// On disk: the only place a durability or memory-budget question can be
    /// asked at all.
    Disk,
}

impl Backend {
    const fn name(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Disk => "disk",
        }
    }
}

/// What the command line asked for.
#[derive(Debug)]
struct Asked {
    backend: Backend,
    only: Option<String>,
    baseline: Option<PathBuf>,
    list: bool,
}

fn main() -> ExitCode {
    let asked = match parse(env::args().skip(1)) {
        Ok(asked) => asked,
        Err(complaint) => {
            eprintln!("{complaint}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    if asked.list {
        for held in workload::ALL {
            println!("{:<12} {}", held.name, held.about);
        }
        return ExitCode::SUCCESS;
    }
    match run(&asked) {
        Ok(()) => ExitCode::SUCCESS,
        Err(complaint) => {
            eprintln!("{complaint}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage: tessari-bench [--backend memory|disk] [--workload <name>] [--baseline <path>] [--list]";

/// Read the arguments, refusing anything unrecognised.
///
/// Written by hand for the same reason the JSON encoder and the base64 decoder
/// are: it is twenty lines with a fixed shape, and a dependency here would be
/// one more thing in the tree that `cargo deny` has to have an opinion about.
///
/// An unknown flag is an **error** rather than something ignored: a benchmark
/// run with a misspelled option that silently measured the default is how a
/// number ends up in a document describing something else.
fn parse(arguments: impl Iterator<Item = String>) -> Result<Asked, String> {
    let mut asked = Asked {
        backend: Backend::Memory,
        only: None,
        baseline: None,
        list: false,
    };
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--list" => asked.list = true,
            "--backend" => {
                asked.backend = match arguments.next().as_deref() {
                    Some("memory") => Backend::Memory,
                    Some("disk") => Backend::Disk,
                    other => {
                        return Err(format!("--backend wants memory or disk, not {other:?}"));
                    }
                };
            }
            "--workload" => {
                let name = arguments
                    .next()
                    .ok_or_else(|| "--workload wants a name".to_owned())?;
                if workload::by_name(&name).is_none() {
                    return Err(format!("no workload called {name:?}; try --list"));
                }
                asked.only = Some(name);
            }
            "--baseline" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--baseline wants a path".to_owned())?;
                asked.baseline = Some(PathBuf::from(path));
            }
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(asked)
}

/// A temporary directory that removes itself.
///
/// The harness must not leave a store behind when a run fails, because the next
/// run would then measure a warm one — and the difference between the two is
/// exactly the kind of thing somebody would later attribute to a code change.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> io::Result<Self> {
        let mut path = env::temp_dir();
        // No randomness available without a dependency, and none needed: the
        // directory is removed before it is created, so a leftover from an
        // earlier crash cannot warm this run either.
        path.push(format!("tessari-bench-{label}"));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A failure to clean up must not mask the failure that led here.
        drop(fs::remove_dir_all(&self.path));
    }
}

fn run(asked: &Asked) -> Result<(), String> {
    let chosen: Vec<&Workload> = match &asked.only {
        Some(name) => workload::by_name(name).into_iter().collect(),
        None => workload::ALL.iter().collect(),
    };

    let mut lines = Vec::new();
    for held in chosen {
        let reports = measure(held, asked.backend)?;
        lines.push((held.name, held.about, reports));
    }

    let mut out = String::new();
    out.push_str(&preamble(asked.backend));
    for (name, about, reports) in &lines {
        out.push_str(&format!("\n## {name}\n\n{about}\n\n"));
        out.push_str(Report::header());
        out.push('\n');
        for report in reports {
            out.push_str(&report.row());
            out.push('\n');
        }
    }

    print!("{out}");
    drop(io::stdout().flush());

    if let Some(path) = &asked.baseline {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|failure| failure.to_string())?;
        }
        fs::write(path, &out).map_err(|failure| failure.to_string())?;
        eprintln!("\nbaseline written to {}", path.display());
    }
    Ok(())
}

/// Run one workload against a database of the chosen kind.
fn measure(held: &Workload, backend: Backend) -> Result<Vec<Report>, String> {
    match backend {
        Backend::Memory => {
            let db = Db::in_memory().map_err(|failure| failure.to_string())?;
            (held.run)(&db).map_err(|failure| failure.to_string())
        }
        Backend::Disk => {
            let scratch = Scratch::new(held.name).map_err(|failure| failure.to_string())?;
            let db = Db::open(scratch.path()).map_err(|failure| failure.to_string())?;
            let reports = (held.run)(&db).map_err(|failure| failure.to_string());
            // The database is dropped before the directory, so the engine has
            // closed its files by the time they are removed.
            drop(db);
            reports
        }
    }
}

/// What every reader of these numbers needs to know before reading them.
fn preamble(backend: Backend) -> String {
    let profile = if cfg!(debug_assertions) {
        "**DEBUG BUILD — these numbers describe the debug build and not the store.** \
         Re-run with `--release`."
    } else {
        "release build"
    };
    // Named in the preamble for the same reason the debug build is: a timing
    // taken with a counter in front of every allocation is comparable with
    // another taken the same way, and with nothing else.
    let counting = if cfg!(feature = "counting") {
        " (counting allocator installed — timings are not comparable with an ordinary build)"
    } else {
        ""
    };
    format!(
        "# TessariDB benchmark\n\n\
         - backend: `{}`\n\
         - build: {}{counting}\n\
         - machine: `{}` / `{}`\n\
         - records per workload: {}\n\
         - percentiles: nearest-rank over every retained sample, per phase\n\n\
         A baseline is comparable with another taken on the same machine and the same\n\
         build profile, and with nothing else. It is read by a person; it is not a gate.\n",
        backend.name(),
        profile,
        env::consts::OS,
        env::consts::ARCH,
        workload::records(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{Backend, Scratch, parse};

    fn asked(arguments: &[&str]) -> Result<super::Asked, String> {
        parse(arguments.iter().map(|held| (*held).to_owned()))
    }

    #[test]
    fn the_default_is_in_memory_and_every_workload() {
        let held = asked(&[]).expect("defaults");
        assert_eq!(held.backend, Backend::Memory);
        assert!(held.only.is_none());
        assert!(held.baseline.is_none());
    }

    #[test]
    fn a_misspelled_option_is_refused_rather_than_ignored() {
        // A run with a misspelled flag that silently measured the default is how
        // a number ends up in a document describing something else.
        assert!(asked(&["--backends", "disk"]).is_err());
        assert!(asked(&["--backend", "ssd"]).is_err());
        assert!(asked(&["--workload", "nonesuch"]).is_err());
    }

    #[test]
    fn an_option_missing_its_value_is_refused() {
        assert!(asked(&["--workload"]).is_err());
        assert!(asked(&["--baseline"]).is_err());
        assert!(asked(&["--backend"]).is_err());
    }

    #[test]
    fn every_named_workload_is_accepted() {
        for held in super::workload::ALL {
            let parsed = asked(&["--workload", held.name]).expect("a workload");
            assert_eq!(parsed.only.as_deref(), Some(held.name));
        }
    }

    #[test]
    fn a_scratch_directory_removes_itself() {
        let path = {
            let scratch = Scratch::new("self-test").expect("a directory");
            let held = scratch.path().to_path_buf();
            assert!(held.is_dir());
            held
        };
        assert!(!path.exists(), "the scratch directory outlived its guard");
    }

    #[test]
    fn a_leftover_directory_does_not_warm_the_next_run() {
        // A store left behind by a crashed run would make the next one measure a
        // warm engine, and the difference would later be attributed to a code
        // change.
        let first = Scratch::new("leftover").expect("a directory");
        std::fs::write(first.path().join("stale"), b"x").expect("a file");
        let path = first.path().to_path_buf();
        core::mem::forget(first);
        assert!(path.join("stale").exists());

        let second = Scratch::new("leftover").expect("a directory");
        assert!(!second.path().join("stale").exists());
    }
}
