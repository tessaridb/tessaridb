#![allow(clippy::panic)]

use std::io::Cursor;

use tessaridb::{Db, Parameters};

use super::{Ended, Given, HELP, Lines, Mode, Piped, Scanner, Write, closed, run, scan, shorthand};
use crate::store::Embedded;

#[path = "tests/output.rs"]
mod output;
#[path = "tests/scanning.rs"]
mod scanning;
#[path = "tests/writes.rs"]
mod writes;

/// A reader that hands over exactly what a test says, in order.
///
/// The abandon path has no other way in: it is a keystroke, and a keystroke
/// cannot be spelt in a `Cursor` full of statements.
struct Scripted(std::collections::VecDeque<Given>);

impl Lines for Scripted {
    fn next(&mut self, _prompt: &str, _out: &mut dyn Write) -> std::io::Result<Given> {
        Ok(self.0.pop_front().unwrap_or(Given::Ended))
    }
}

#[test]
fn abandoning_throws_away_the_half_typed_statement_and_nothing_else() {
    // Ctrl-C at the continuation prompt. What must NOT happen is the
    // fragment joining the next statement, and what must also not happen is
    // "input ended inside an unfinished statement" at the end — the input
    // did not end, it was withdrawn.
    let db = Db::in_memory().expect("a database");
    let mut store = Embedded::new(&db, None, Parameters::new()).expect("a session");
    let mut input = Scripted(
        [
            Given::Line("CREATE users:1 = {\n".to_owned()),
            Given::Abandon,
            Given::Line("DEFINE NAMESPACE prod;\n".to_owned()),
            Given::Ended,
        ]
        .into(),
    );
    let mut out = Vec::new();
    let ended = run(&mut store, &mut input, &mut out, Mode::Interactive).expect("a run");
    let said = String::from_utf8(out).expect("text");

    assert_eq!(ended, Ended::Fine, "{said}");
    assert_eq!(
        said.trim(),
        "ok",
        "the statement after the abandon ran, and it ran alone"
    );
    assert!(
        !said.contains("unfinished"),
        "the fragment was withdrawn, not left dangling: {said}"
    );
}

fn ran(script: &str, mode: Mode) -> (String, Ended) {
    let db = Db::in_memory().expect("a database");
    let mut store = Embedded::new(&db, None, Parameters::new()).expect("a session");
    let mut input = Piped::new(Cursor::new(script.as_bytes().to_vec()));
    let mut out = Vec::new();
    let ended = run(&mut store, &mut input, &mut out, mode).expect("a run");
    (String::from_utf8(out).expect("text"), ended)
}

/// The tenancy every write below needs, and nothing else.
const READY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION users;
DEFINE COLLECTION sessions IDENTITY uuid;
";

/// The lines a script produced that are not `ok` — what was actually said.
fn said(script: &str) -> Vec<String> {
    let (out, ended) = ran(&format!("{READY}{script}"), Mode::Script);
    assert_eq!(ended, Ended::Fine, "{out}");
    out.lines()
        .filter(|line| *line != "ok")
        .map(ToOwned::to_owned)
        .collect()
}

/// A store with one flat record in it, ready to be selected from.
const ONE_RECORD: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE COLLECTION users;\n\
CREATE users:1 = { name: 'ada' };\n\
SELECT * FROM users:1;\n";
