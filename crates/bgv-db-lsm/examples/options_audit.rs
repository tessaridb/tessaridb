//! Print the options a store is actually running with.
//!
//! The engine writes an `OPTIONS-<n>` file into the store directory on every
//! open, and that file — not the configuration struct in this repo — is the
//! ground truth for an audit. The struct records what the code meant to set,
//! which is a different question and the one that hides a setter the release
//! ignores.
//!
//! ```text
//! cargo run -p bgv-db-lsm --example options_audit -- <store-path>
//! ```
//!
//! With no path, a store is created in a temporary directory, opened with the
//! defaults, and its file printed — which is what an audit of the *shipped*
//! configuration needs.

use std::path::PathBuf;
use std::process::ExitCode;

use bgv_db_lsm::{LsmBackend, StoreConfig, effective_options_files};

fn main() -> ExitCode {
    let argument = std::env::args().nth(1);
    let (path, temporary) = match argument {
        Some(given) => (PathBuf::from(given), false),
        None => (
            std::env::temp_dir().join(format!("bgv-db-options-audit-{}", std::process::id())),
            true,
        ),
    };

    // Opening is what makes the engine write the file; reading a directory that
    // was never opened would report the options of nothing.
    let opened = LsmBackend::open(&path, StoreConfig::default());
    if let Err(error) = opened {
        eprintln!("could not open a store at {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    drop(opened);

    let files = match effective_options_files(&path) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("could not list the options files: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(latest) = files.last() else {
        eprintln!("the store wrote no OPTIONS file, which should not happen");
        return ExitCode::FAILURE;
    };

    match std::fs::read_to_string(latest) {
        Ok(contents) => {
            println!("# source: {}", latest.display());
            println!("{contents}");
        }
        Err(error) => {
            eprintln!("could not read {}: {error}", latest.display());
            return ExitCode::FAILURE;
        }
    }

    if temporary {
        let _ = std::fs::remove_dir_all(&path);
    }
    ExitCode::SUCCESS
}
