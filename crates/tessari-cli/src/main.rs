//! `tessaridb` — a command line for TessariDB.
//!
//! ```text
//! tessaridb                                    an in-memory store, and a prompt
//! tessaridb ./data                             a store on disk, and a prompt
//! tessaridb ./data -e 'SELECT * FROM users;'   one script, then exit
//! tessaridb ./data -f setup.tessariql          a file
//! echo 'SELECT …' | tessaridb ./data           a pipe
//! tessaridb --at 127.0.0.1:7654                a running node, and a prompt
//! tessaridb ./data --serve 0.0.0.0:7654        be that node
//! tessaridb ./data --serve :7654 --http :8000  be that node on both surfaces
//! ```
//!
//! # A path or an address, and the same prompt over either
//!
//! With a path it opens the store **in this process**, through the embedded
//! facade. With `--at` it talks to a running node over the wire protocol. Both
//! produce the same answers to the same renderer, so what is printed does not
//! depend on which one was used — see `store.rs` for why that is the shape of
//! the code rather than a claim about it.
//!
//! It is `--at` and not `--url` because this protocol has no scheme, and calling
//! an address a URL promises one. It is the same binary and not a second program
//! for the same reason `--serve` is: what changes is where the store is, and
//! that is an argument.
//!
//! Answers are printed in **TessariQL's own syntax**, so what comes out can be
//! pasted back in. JSON is what the HTTP endpoint speaks, and it had to decide
//! how seventeen types become six; a terminal is owed no such compromise.
//!
//! There is line editing and per-session history, written here rather than
//! taken as a dependency: `line.rs` says why, and `raw.rs` says what it costs.
//! Nothing is written to disk, because statements carry passwords.

// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

mod arguments;
mod balancing_round;
mod bootstrap;
mod collection_round;
mod consumers;
mod credentials;
mod greeting_round;
mod housekeeping;
mod leadership_round;
mod line;
mod logging;
mod maintenance;
mod peer_door;
mod peers;
mod presented;
mod raw;
mod reseeding;
/// How a value is written back as TessariQL — the language's own, so a state
/// script and this command line write one value the same way.
use tessari_ql::literal as render;
mod runtime;
mod scanner;
mod serving;
mod session;
mod settling_round;
mod shutdown;
mod store;
mod streaming;
mod supervise;
mod table;
mod tls;

use std::env;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::process::ExitCode;

use tessaridb::Db;

use crate::arguments::{Asked, Source, credentials, parse};
use crate::maintenance::{backup, dump, health, restore, snapshot, verify};
use crate::serving::serve;
use crate::session::{Ended, Mode};

// A panic ends the unit of work that met it — a connection, a request, one
// round of a cadence — and not the node; `supervise.rs` says where that holds
// and where it deliberately does not. All of it needs the build to unwind.
#[cfg(panic = "abort")]
compile_error!(
    "tessaridb contains panics per connection and per cadence; build with panic = \"unwind\""
);

fn main() -> ExitCode {
    // Before anything that could have something to report. A second logger
    // installed by an embedding caller would already have won, and that is the
    // right outcome — this one belongs to the binary.
    drop(logging::install());
    supervise::log_panics();
    let asked = match parse(env::args().skip(1)) {
        Ok(asked) => asked,
        Err(complaint) => {
            eprintln!("{complaint}");
            return ExitCode::FAILURE;
        }
    };
    match run(asked) {
        Ok(Ended::Fine) => ExitCode::SUCCESS,
        // A refusal is an answer, and an exit code is how a shell reads one.
        Ok(Ended::Refused) => ExitCode::FAILURE,
        Err(complaint) => {
            eprintln!("tessaridb: {complaint}");
            ExitCode::FAILURE
        }
    }
}

fn run(asked: Asked) -> Result<Ended, String> {
    // Before the store is opened, because opening it is the slow part and a
    // node recovering a large log would otherwise report an uptime that began
    // after the interval a restart-detector most wants to see.
    let started = std::time::Instant::now();
    let credentials = credentials(asked.user)?;
    let parameters = asked.parameters;
    let sequence = asked.at_sequence;

    // Saying which build this is, or what the flags are, touches nothing at
    // all, so both come before even the address: they have to answer on a
    // machine with no store, no node to reach and no password to hand over.
    // Standard output and a successful exit, because both are answers rather
    // than refusals — a `--help` on standard error with a non-zero status is
    // one a pipeline cannot read and a packaging check fails on.
    match asked.source {
        Source::Version => {
            println!("tessaridb {}", tessaridb::BUILD_VERSION);
            return Ok(Ended::Fine);
        }
        Source::Help => {
            println!("{}", crate::arguments::USAGE);
            return Ok(Ended::Fine);
        }
        _ => {}
    }

    // Verifying reads a file and touches no store, so it happens before one is
    // opened — which is what makes it usable on a machine that has nothing but
    // the backup.
    let at_rest = encryption_key(
        asked.encryption_key.as_deref(),
        asked.store.is_some() || matches!(asked.source, Source::Verify(_)),
    )?;
    // The key a backup being read was sealed under: named, or the store's.
    let backup_key = asked
        .backup_key
        .as_deref()
        .map(|path| tessaridb::AtRestKey::read(path).map_err(|failure| failure.to_string()))
        .transpose()?;
    if let Source::Verify(path) = &asked.source {
        return verify(path, backup_key.as_ref().or(at_rest.as_ref()));
    }
    if let Some(address) = &asked.at {
        // The flag, or the environment beside the other `--at` settings; given,
        // the node is spoken to over TLS and must prove itself by it.
        let trusting = match asked.authority.clone().or_else(|| {
            std::env::var(tls::TLS_AUTHORITY)
                .ok()
                .map(std::path::PathBuf::from)
        }) {
            Some(file) => Some(tls::authority(&file)?),
            None => None,
        };
        let mut remote = store::Remote::connect(address, trusting, credentials, parameters)?;
        let mut out = io::stdout().lock();
        return statements(&mut remote, &mut out, &asked.source, Where::Node(address));
    }

    let db = match &asked.store {
        Some(path) => Db::open_encrypted(path, tessaridb::StoreConfig::default(), at_rest)
            .map_err(|failure| format!("{}: {failure}", path.display()))?,
        None => Db::in_memory().map_err(|failure| failure.to_string())?,
    };

    // These are store operations rather than statements, so they never reach a
    // session at all — and an operator rehearses a restore with a command, which
    // is what "rehearsed" in the readiness checklist means.
    match &asked.source {
        // A snapshot unless a position asks for the log (ADR-0094 D1): only a log
        // has a `--from`, and `--from 1` is the whole of it.
        Source::Backup(path) => {
            return match sequence {
                None => snapshot(&db, path),
                Some(_) => backup(&db, path, sequence),
            }
            .map(|()| Ended::Fine);
        }
        Source::Snapshot(path) => return snapshot(&db, path).map(|()| Ended::Fine),
        Source::Dump(path) => return dump(&db, path).map(|()| Ended::Fine),
        Source::Restore(path) => {
            return restore(&db, path, sequence, backup_key.as_ref()).map(|()| Ended::Fine);
        }
        Source::Health => return health(&db),
        Source::Serve => return serve(db, &asked.serving, asked.cluster.as_ref(), started),
        Source::Verify(_)
        | Source::Version
        | Source::Help
        | Source::Standard
        | Source::Inline(_)
        | Source::File(_) => {}
    }

    let mut embedded = store::Embedded::new(&db, credentials.as_ref(), parameters)?;
    let mut out = io::stdout().lock();
    let opened = asked.store.as_deref();
    statements(&mut embedded, &mut out, &asked.source, Where::Store(opened))
}

/// What the greeting says was opened.
#[derive(Debug, Clone, Copy)]
enum Where<'a> {
    /// A store in this process, or none when it is in memory.
    Store(Option<&'a std::path::Path>),
    /// A node at this address.
    Node(&'a str),
}

/// Read statements from wherever they come from and run them.
///
/// One function for both, because where the store is changes nothing about
/// where a statement ends or what a refusal does.
fn statements(
    store: &mut dyn store::Store,
    out: &mut impl Write,
    source: &Source,
    opened: Where<'_>,
) -> Result<Ended, String> {
    let ended = match source {
        Source::Inline(script) => {
            let mut input = session::Piped::new(io::Cursor::new(script.clone().into_bytes()));
            session::run(store, &mut input, out, Mode::Script)
        }
        Source::File(path) => {
            // Read rather than streamed, so a missing or unreadable file is one
            // clear failure before anything runs instead of a partial script.
            let held =
                fs::read(path).map_err(|failure| format!("{}: {failure}", path.display()))?;
            let mut input = session::Piped::new(io::Cursor::new(held));
            session::run(store, &mut input, out, Mode::Script)
        }
        Source::Backup(_)
        | Source::Snapshot(_)
        | Source::Dump(_)
        | Source::Restore(_)
        | Source::Verify(_)
        | Source::Version
        | Source::Help
        | Source::Health
        | Source::Serve => {
            // Resolved before this function is reached, for the embedded path,
            // and refused during parsing for a node.
            return Ok(Ended::Fine);
        }
        Source::Standard => {
            let stdin = io::stdin();
            // A prompt is for a person. Piped input gets none, so the output is
            // a script's output and not a transcript.
            let mode = if stdin.is_terminal() {
                greet(out, opened).map_err(|failure| failure.to_string())?;
                Mode::Interactive
            } else {
                Mode::Script
            };
            // A person gets the editor; a pipe gets the reader it always had.
            // `attach` answers `None` for anything that is not a terminal, so
            // the two conditions cannot come apart.
            let ended = match line::Edited::attach() {
                Some(mut edited) if mode == Mode::Interactive => {
                    session::run(store, &mut edited, out, mode)
                }
                _ => {
                    let mut input = session::Piped::new(BufReader::new(stdin.lock()));
                    session::run(store, &mut input, out, mode)
                }
            };
            if mode == Mode::Interactive && ended.is_ok() {
                // End-of-input at a prompt leaves the cursor mid-line.
                drop(writeln!(out));
            }
            ended
        }
    };
    ended.map_err(|failure| failure.to_string())
}

/// One line saying what was opened, because "which store am I in" is the first
/// thing anybody wonders at a prompt.
fn greet(out: &mut impl Write, opened: Where<'_>) -> io::Result<()> {
    match opened {
        Where::Store(Some(path)) => writeln!(out, "tessaridb — {}", path.display())?,
        Where::Store(None) => writeln!(out, "tessaridb — in memory; nothing written here is kept")?,
        Where::Node(address) => writeln!(out, "tessaridb — {address}")?,
    }
    writeln!(out, "`.help` for the little there is of it")
}

/// Where the key to the data at rest is read from when the flag names none.
const ENCRYPTION_KEY_FILE: &str = "TESSARIDB_ENCRYPTION_KEY_FILE";

/// The key the store and its backups are encrypted under (ADR-0108 D7): the
/// flag's file, else `TESSARIDB_ENCRYPTION_KEY_FILE` for a store on disk or a
/// backup to verify, else none.
fn encryption_key(
    named: Option<&std::path::Path>,
    writes_files: bool,
) -> Result<Option<tessaridb::AtRestKey>, String> {
    let file = match named {
        Some(path) => Some(path.to_path_buf()),
        // The variable stands for the flag only where the flag means
        // something, so a container that sets it can still run `--at`.
        None if writes_files => std::env::var_os(ENCRYPTION_KEY_FILE).map(std::path::PathBuf::from),
        None => None,
    };
    file.map(|path| tessaridb::AtRestKey::read(&path).map_err(|failure| failure.to_string()))
        .transpose()
}
