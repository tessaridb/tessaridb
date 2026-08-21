//! `bgv` — a command line for `bgv-db`.
//!
//! ```text
//! bgv                                    an in-memory store, and a prompt
//! bgv ./data                             a store on disk, and a prompt
//! bgv ./data -e 'SELECT * FROM users;'   one script, then exit
//! bgv ./data -f setup.bgvql              a file
//! echo 'SELECT …' | bgv ./data           a pipe
//! ```
//!
//! # What it is, and what it is not yet
//!
//! It opens the store **in this process**, through the embedded facade. A client
//! that talks to a running node over HTTP is the other half of this node and is
//! not built; when it is, it is `--url` beside the path rather than a second
//! program.
//!
//! Answers are printed in **bgvQL's own syntax**, so what comes out can be
//! pasted back in. JSON is what the HTTP endpoint speaks, and it had to decide
//! how fifteen types become six; a terminal is owed no such compromise.
//!
//! There is no line editing and no history. Both mean a dependency, and a
//! terminal library is a large surface to take for a convenience — so it is
//! stated in `.help` rather than left to be discovered by pressing up.

mod render;
mod session;

use std::env;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bgv_db::Db;

use crate::session::{Ended, Mode};

const USAGE: &str = "\
usage: bgv [<path>] [-e <script> | -f <file>]

  <path>          a store on disk; omitted, the store is in memory and is lost
  -e <script>     run this and exit
  -f <file>       run this file and exit
  --help          this

with neither -e nor -f, statements are read from standard input: a prompt when
that is a terminal, a script when it is a pipe.";

/// What the command line asked for.
#[derive(Debug)]
struct Asked {
    store: Option<PathBuf>,
    source: Source,
}

/// Where the statements come from.
#[derive(Debug)]
enum Source {
    /// Standard input, prompting or not depending on what it is.
    Standard,
    /// One script given on the command line.
    Inline(String),
    /// A file.
    File(PathBuf),
}

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

/// Read the arguments, refusing anything unrecognised.
///
/// An unknown option is an error rather than something ignored: a session opened
/// with a misspelled flag that silently used the default is how somebody writes
/// to the wrong store.
fn parse(arguments: impl Iterator<Item = String>) -> Result<Asked, String> {
    let mut store = None;
    let mut source = Source::Standard;
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Err(USAGE.to_owned()),
            "-e" | "--execute" => {
                let script = arguments
                    .next()
                    .ok_or_else(|| "-e wants a script".to_owned())?;
                source = Source::Inline(script);
            }
            "-f" | "--file" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "-f wants a path".to_owned())?;
                source = Source::File(PathBuf::from(path));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other:?}\n\n{USAGE}"));
            }
            path if store.is_none() => store = Some(PathBuf::from(path)),
            extra => {
                return Err(format!(
                    "only one store may be opened, and {extra:?} is a second"
                ));
            }
        }
    }
    Ok(Asked { store, source })
}

fn run(asked: Asked) -> Result<Ended, String> {
    let db = match &asked.store {
        Some(path) => Db::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
        None => Db::in_memory().map_err(|failure| failure.to_string())?,
    };
    let mut out = io::stdout().lock();

    let ended = match asked.source {
        Source::Inline(script) => {
            let mut input = io::Cursor::new(script.into_bytes());
            session::run(&db, &mut input, &mut out, Mode::Script)
        }
        Source::File(path) => {
            // Read rather than streamed, so a missing or unreadable file is one
            // clear failure before anything runs instead of a partial script.
            let held =
                fs::read(&path).map_err(|failure| format!("{}: {failure}", path.display()))?;
            let mut input = io::Cursor::new(held);
            session::run(&db, &mut input, &mut out, Mode::Script)
        }
        Source::Standard => {
            let stdin = io::stdin();
            // A prompt is for a person. Piped input gets none, so the output is
            // a script's output and not a transcript.
            let mode = if stdin.is_terminal() {
                greet(&mut out, asked.store.as_deref()).map_err(|failure| failure.to_string())?;
                Mode::Interactive
            } else {
                Mode::Script
            };
            let mut input = BufReader::new(stdin.lock());
            let ended = session::run(&db, &mut input, &mut out, mode);
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
fn greet(out: &mut impl Write, store: Option<&std::path::Path>) -> io::Result<()> {
    match store {
        Some(path) => writeln!(out, "bgv — {}", path.display())?,
        None => writeln!(out, "bgv — in memory; nothing written here is kept")?,
    }
    writeln!(out, "`.help` for the little there is of it")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{Source, parse};

    fn asked(arguments: &[&str]) -> Result<super::Asked, String> {
        parse(arguments.iter().map(|held| (*held).to_owned()))
    }

    #[test]
    fn no_arguments_is_an_in_memory_store_read_from_standard_input() {
        let held = asked(&[]).expect("defaults");
        assert!(held.store.is_none());
        assert!(matches!(held.source, Source::Standard));
    }

    #[test]
    fn a_bare_argument_is_the_store() {
        let held = asked(&["./data"]).expect("a path");
        assert_eq!(held.store.as_deref(), Some(std::path::Path::new("./data")));
    }

    #[test]
    fn a_script_and_a_file_are_told_apart() {
        assert!(matches!(
            asked(&["-e", "SELECT * FROM users;"])
                .expect("a script")
                .source,
            Source::Inline(_)
        ));
        assert!(matches!(
            asked(&["-f", "setup.bgvql"]).expect("a file").source,
            Source::File(_)
        ));
    }

    #[test]
    fn a_misspelled_option_is_refused_rather_than_read_as_a_path() {
        // Otherwise `--excute 'DELETE …'` opens a store called `--excute`.
        assert!(asked(&["--excute", "x"]).is_err());
        assert!(asked(&["-e"]).is_err());
        assert!(asked(&["-f"]).is_err());
    }

    #[test]
    fn a_second_store_is_refused_rather_than_silently_ignored() {
        assert!(asked(&["./one", "./two"]).is_err());
    }
}
