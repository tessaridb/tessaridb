//! `bgv` — a command line for `bgv-db`.
//!
//! ```text
//! bgv                                    an in-memory store, and a prompt
//! bgv ./data                             a store on disk, and a prompt
//! bgv ./data -e 'SELECT * FROM users;'   one script, then exit
//! bgv ./data -f setup.bgvql              a file
//! echo 'SELECT …' | bgv ./data           a pipe
//! bgv --at 127.0.0.1:7654                a running node, and a prompt
//! bgv ./data --serve 0.0.0.0:7654        be that node
//! bgv ./data --serve :7654 --http :8000  be that node on both surfaces
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
//! Answers are printed in **bgvQL's own syntax**, so what comes out can be
//! pasted back in. JSON is what the HTTP endpoint speaks, and it had to decide
//! how fifteen types become six; a terminal is owed no such compromise.
//!
//! There is no line editing and no history. Both mean a dependency, and a
//! terminal library is a large surface to take for a convenience — so it is
//! stated in `.help` rather than left to be discovered by pressing up.

mod arguments;
mod render;
mod session;
mod store;

use std::env;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::process::ExitCode;

use bgv_db::Db;

use crate::arguments::{Asked, Serving, Source, credentials, parse};
use crate::session::{Ended, Mode};

fn main() -> ExitCode {
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
            eprintln!("bgv: {complaint}");
            ExitCode::FAILURE
        }
    }
}

fn run(asked: Asked) -> Result<Ended, String> {
    let credentials = credentials(asked.user)?;
    let parameters = asked.parameters;
    let sequence = asked.at_sequence;

    // Verifying reads a file and touches no store, so it happens before one is
    // opened — which is what makes it usable on a machine that has nothing but
    // the backup.
    if let Source::Verify(path) = &asked.source {
        return verify(path);
    }
    if let Some(address) = &asked.at {
        let mut remote = store::Remote::connect(address, credentials, parameters)?;
        let mut out = io::stdout().lock();
        return statements(&mut remote, &mut out, &asked.source, Where::Node(address));
    }

    let db = match &asked.store {
        Some(path) => Db::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
        None => Db::in_memory().map_err(|failure| failure.to_string())?,
    };

    // These are store operations rather than statements, so they never reach a
    // session at all — and an operator rehearses a restore with a command, which
    // is what "rehearsed" in the readiness checklist means.
    match &asked.source {
        Source::Backup(path) => return backup(&db, path, sequence).map(|()| Ended::Fine),
        Source::Restore(path) => return restore(&db, path, sequence).map(|()| Ended::Fine),
        Source::Health => return health(&db),
        Source::Serve => return serve(db, &asked.serving),
        Source::Verify(_) | Source::Standard | Source::Inline(_) | Source::File(_) => {}
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
            let mut input = io::Cursor::new(script.clone().into_bytes());
            session::run(store, &mut input, out, Mode::Script)
        }
        Source::File(path) => {
            // Read rather than streamed, so a missing or unreadable file is one
            // clear failure before anything runs instead of a partial script.
            let held =
                fs::read(path).map_err(|failure| format!("{}: {failure}", path.display()))?;
            let mut input = io::Cursor::new(held);
            session::run(store, &mut input, out, Mode::Script)
        }
        Source::Backup(_)
        | Source::Restore(_)
        | Source::Verify(_)
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
            let mut input = BufReader::new(stdin.lock());
            let ended = session::run(store, &mut input, out, mode);
            if mode == Mode::Interactive && ended.is_ok() {
                // End-of-input at a prompt leaves the cursor mid-line.
                drop(writeln!(out));
            }
            ended
        }
    };
    ended.map_err(|failure| failure.to_string())
}

/// Be the node the other half of this program connects to.
///
/// The same binary rather than a second one: what changes is where the store is,
/// and that is an argument. It serves until it is stopped, so it never returns
/// on the happy path.
fn serve(db: Db, serving: &Serving) -> Result<Ended, String> {
    let db = std::sync::Arc::new(db);
    // Both are bound before either serves, so an address that cannot be taken
    // is a failure to start rather than a surface that quietly went missing
    // while the other one answered.
    let wire = match &serving.wire {
        Some(address) => Some(
            bgv_db_wire::Node::bind(std::sync::Arc::clone(&db), address.as_str())
                .map_err(|failure| format!("{address}: {failure}"))?,
        ),
        None => None,
    };
    let http = match &serving.http {
        Some(address) => Some(
            bgv_db_http::Node::bind(std::sync::Arc::clone(&db), address)
                .map_err(|failure| format!("{address}: {failure}"))?,
        ),
        None => None,
    };

    // On the error stream, so a node whose output is being piped somewhere still
    // tells a person at the terminal that it came up and where. What was *bound*
    // rather than what was asked for, which is what makes `:0` usable.
    if let Some(node) = &wire {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        eprintln!("bgv — wire protocol on {bound}");
    }
    if let Some(node) = &http {
        eprintln!("bgv — http on {}", node.address());
    }
    eprintln!("bgv — there is no TLS, so trust the network");

    match (wire, http) {
        // A thread for one and this thread for the other: two listeners, one
        // store, and no runtime to hold them.
        (Some(wire), Some(http)) => std::thread::scope(|scope| {
            scope.spawn(|| http.serve());
            wire.serve();
        }),
        (Some(wire), None) => wire.serve(),
        (None, Some(http)) => http.serve(),
        // Unreachable through the parser, which sets `Source::Serve` only when
        // an address was given — said here rather than assumed, because the two
        // are far enough apart to drift.
        (None, None) => return Err("--serve or --http wants an address".to_owned()),
    }
    Ok(Ended::Fine)
}

/// Write the store's log to a file.
///
/// The whole store, because state is a pure function of the log — so this is a
/// complete backup and not a partial one, and restoring it is a replay.
fn backup(db: &Db, path: &std::path::Path, from: Option<u64>) -> Result<(), String> {
    let mut out = std::io::BufWriter::new(
        fs::File::create(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    let from = bgv_db::Sequence::new(from.unwrap_or(1));
    let written = bgv_db_backup::write_from(db.store(), &mut out, from)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    out.flush()
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!(
        "{} record(s) to {}, sequences {}..={}",
        written.records,
        path.display(),
        written.from,
        written.tail
    );
    Ok(())
}

/// Replay a file into an empty store.
fn restore(db: &Db, path: &std::path::Path, upto: Option<u64>) -> Result<(), String> {
    let mut input = std::io::BufReader::new(
        fs::File::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    let upto = upto.map(bgv_db::Sequence::new);
    let held = bgv_db_backup::read_until(db.store(), &mut input, upto)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!("{} record(s) from {}", held.records, path.display());
    if held.truncated {
        // Said loudly and on the error stream, because a partial restore that
        // reads as a success is how somebody learns later that the last hour is
        // gone.
        eprintln!(
            "warning: {} was cut short — it says it holds {} record(s) and {} were read",
            path.display(),
            held.tail,
            held.records
        );
    }
    Ok(())
}

/// Read a backup and say what it holds, applying none of it.
fn verify(path: &std::path::Path) -> Result<Ended, String> {
    let mut input = std::io::BufReader::new(
        fs::File::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    let held = bgv_db_backup::verify(&mut input)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!(
        "{} record(s), sequences {}..={}, good through {}",
        held.records, held.from, held.tail, held.good_through
    );
    if held.truncated {
        // On the error stream and with a non-zero exit, because the whole point
        // of verifying is that somebody's script can act on the answer.
        eprintln!(
            "warning: {} was cut short — it says it holds through {} and reads through {}",
            path.display(),
            held.tail,
            held.good_through
        );
        return Ok(Ended::Refused);
    }
    Ok(Ended::Fine)
}

/// Say whether the store is well.
///
/// The same question `GET /health` answers, for an operator holding a store and
/// no server — which is exactly the situation somebody is in when they are
/// wondering whether it is still keeping their data. Exits non-zero when it is
/// not, so a cron line needs no parsing.
fn health(db: &Db) -> Result<Ended, String> {
    let held = db.store().health().map_err(|failure| failure.to_string())?;
    match held.complaint() {
        None => {
            println!("well — committed to sequence {}", held.committed);
            Ok(Ended::Fine)
        }
        Some(said) => {
            println!("unwell — {said}");
            Ok(Ended::Refused)
        }
    }
}

/// One line saying what was opened, because "which store am I in" is the first
/// thing anybody wonders at a prompt.
fn greet(out: &mut impl Write, opened: Where<'_>) -> io::Result<()> {
    match opened {
        Where::Store(Some(path)) => writeln!(out, "bgv — {}", path.display())?,
        Where::Store(None) => writeln!(out, "bgv — in memory; nothing written here is kept")?,
        Where::Node(address) => writeln!(out, "bgv — {address}")?,
    }
    writeln!(out, "`.help` for the little there is of it")
}
