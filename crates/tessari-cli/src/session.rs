//! Reading statements from somewhere and running them.
//!
//! # One rule, and it is the corpus's rule
//!
//! Input is accumulated until a `;` **outside a string and outside a comment**
//! closes a statement.
//! The conformance corpus had to solve exactly this — its own splitter has a
//! test called *the statement splitter does not cut a string in half* — and this
//! is the same rule rather than a second one, because two answers to "where does
//! a statement end" is one more than a language can afford.
//!
//! Accumulating rather than executing per line is what lets somebody paste
//! `CREATE users:1 = {` and the fields that follow it, which is what anybody
//! copying from a document will do.
//!
//! # A refusal does not end a session
//!
//! At a prompt, a bad statement prints its message and the next one runs. In a
//! script it stops, because a script is a sequence somebody expected to complete
//! and carrying on past a failed step is how a half-applied migration happens.

use std::io::{BufRead, Write};

use tessari_wire::{Answer, Exact, Suggested};

use crate::render;
use crate::store::Store;
use crate::table::Shape;

/// Whether input is coming from a person or from a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A prompt: a refusal is reported and the session continues.
    Interactive,
    /// A script: a refusal stops the run.
    Script,
}

/// What a reader produced.
///
/// Three cases rather than `Option<String>` because a person at a terminal can
/// do a third thing: throw away a statement they are halfway through typing. A
/// sentinel line would have carried that instead, and a sentinel is a value the
/// language could also produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Given {
    /// A line, with its newline still on it.
    Line(String),
    /// Whatever is half-typed should go, and the prompt start over.
    Abandon,
    /// There is no more input.
    Ended,
}

/// Where a session's lines come from.
///
/// The prompt is drawn *by the reader* rather than by the session, because a
/// reader that owns the terminal has to redraw it on every keystroke and cannot
/// have somebody else deciding when it appears.
pub trait Lines {
    /// The next line, drawing `prompt` when there is one to draw.
    ///
    /// # Errors
    ///
    /// Returns an error only when reading or writing fails.
    fn next(&mut self, prompt: &str, out: &mut dyn Write) -> std::io::Result<Given>;

    /// The next line, drawn without showing what was typed and remembered
    /// nowhere.
    ///
    /// Answers [`Given::Abandon`] when this reader cannot hide the input, which
    /// is the honest failure: reading a passphrase onto a visible screen while
    /// the caller believes it is hidden is worse than not offering the prompt.
    ///
    /// The default reads an ordinary line, which is right for a pipe — there is
    /// no terminal echoing anything, and a script feeding a passphrase in has
    /// already decided where it keeps one.
    ///
    /// # Errors
    ///
    /// Returns an error only when reading or writing fails.
    fn secret(&mut self, prompt: &str, out: &mut dyn Write) -> std::io::Result<Given> {
        self.next(prompt, out)
    }
}

/// Lines from anything that reads: a file, a pipe, a string.
pub struct Piped<R>(R);

impl<R: BufRead> Piped<R> {
    /// Read lines from `source`.
    pub const fn new(source: R) -> Self {
        Self(source)
    }
}

impl<R: BufRead> Lines for Piped<R> {
    fn next(&mut self, prompt: &str, out: &mut dyn Write) -> std::io::Result<Given> {
        if !prompt.is_empty() {
            write!(out, "{prompt}")?;
            out.flush()?;
        }
        let mut line = String::new();
        match self.0.read_line(&mut line)? {
            0 => Ok(Given::Ended),
            _ => Ok(Given::Line(line)),
        }
    }
}

/// What a run ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Everything ran.
    Fine,
    /// Something was refused, and the exit code should say so.
    Refused,
}

/// Read statements from `input`, run them against `db`, write to `out`.
///
/// # Errors
///
/// Returns an error only when reading or writing fails. A refused *statement* is
/// reported through [`Ended`] rather than as an error, because it is an answer.
pub fn run(
    store: &mut dyn Store,
    input: &mut dyn Lines,
    out: &mut impl Write,
    mode: Mode,
) -> std::io::Result<Ended> {
    let mut pending = String::new();
    // Walks `pending` once as it grows, rather than from the start after every
    // line — see [`Scanner`] for the cost that made this necessary.
    let mut scanner = Scanner::default();
    let mut ended = Ended::Fine;
    let mut timing = false;
    // A table cannot be pasted back into a statement, and this program prints
    // TessariQL precisely so that it can be. So the table is for a person at a
    // prompt, and anything piped or scripted keeps the pasteable form — which is
    // also the one a `diff` against a recorded answer expects. `.mode` overrides
    // either way.
    let mut shape = match mode {
        Mode::Interactive => Shape::Auto,
        Mode::Script => Shape::Document,
    };

    loop {
        let prompt = match (mode, pending.is_empty()) {
            (Mode::Script, _) => "",
            (Mode::Interactive, true) => "tessaridb> ",
            (Mode::Interactive, false) => "   > ",
        };
        let line = match input.next(prompt, out)? {
            Given::Line(line) => line,
            // Not an error and not an end: the statement goes, and the next
            // prompt is a first-line prompt again because there is no longer
            // anything unfinished for it to continue.
            Given::Abandon => {
                pending.clear();
                scanner = Scanner::default();
                continue;
            }
            Given::Ended => break,
        };
        let trimmed = line.trim();

        // Dot-commands are read only at the start of a statement, so a `.exit`
        // pasted inside an unfinished object is data rather than a command.
        let script = if pending.is_empty() && trimmed == ".unseal" {
            // The one command that builds a statement out of something typed
            // afterwards rather than out of the line itself. It exists because
            // `UNSEAL VAULT WITH '…'` puts a passphrase on the screen and into
            // this session's history, and the two together mean the next person
            // at the terminal presses the up arrow and reads it.
            match input.secret("passphrase: ", out)? {
                Given::Line(text) => {
                    let held = text.trim_end_matches(['\r', '\n']).to_owned();
                    if held.is_empty() {
                        continue;
                    }
                    format!("UNSEAL VAULT WITH '{}';", quoted(&held))
                }
                Given::Abandon => {
                    writeln!(
                        out,
                        "this terminal cannot hide what is typed; write the \
                         statement in full if that is acceptable"
                    )?;
                    continue;
                }
                Given::Ended => break,
            }
        } else if pending.is_empty() && trimmed.starts_with('.') {
            match shorthand(trimmed) {
                Some(statement) => statement,
                None => {
                    match trimmed.split_once(' ') {
                        Some((".mode", asked)) => match Shape::named(asked.trim()) {
                            Some(chosen) => shape = chosen,
                            None => writeln!(
                                out,
                                "no such mode: {} — auto, table or document",
                                asked.trim()
                            )?,
                        },
                        _ => match trimmed {
                            ".exit" | ".quit" => break,
                            ".help" => writeln!(out, "{HELP}")?,
                            ".mode" => writeln!(out, "{}", shape.name())?,
                            ".timing" => {
                                timing = !timing;
                                writeln!(out, "timing is {}", if timing { "on" } else { "off" })?;
                            }
                            other => writeln!(out, "no such command: {other}\n{HELP}")?,
                        },
                    }
                    continue;
                }
            }
        } else {
            pending.push_str(&line);
            scanner.feed(&line);
            if !scanner.state().closed {
                continue;
            }
            scanner = Scanner::default();
            core::mem::take(&mut pending)
        };
        if script.trim().is_empty() {
            continue;
        }

        let began = std::time::Instant::now();
        match store.run(&script) {
            Ok(answers) => {
                for answer in &answers {
                    report(out, answer, shape)?;
                }
            }
            Err(refusal) => {
                writeln!(out, "error: {refusal}")?;
                ended = Ended::Refused;
                if mode == Mode::Script {
                    return Ok(ended);
                }
            }
        }
        if timing {
            // Wall clock around the whole script, which is what somebody timing
            // a statement is asking about. It includes the round trip when the
            // store is a node, and saying so is the point: that is the number
            // that decides whether a query is slow from where you are sitting.
            writeln!(
                out,
                "time: {:.3} ms",
                began.elapsed().as_secs_f64() * 1000.0
            )?;
        }
    }

    // Input that ran out mid-statement is worth saying: silently discarding it
    // looks exactly like a statement that ran and answered nothing.
    let remainder = scanner.state();
    if remainder.open_transaction {
        // Hand it over rather than describing it. The store is what discards
        // the work of a transaction nobody closed, so the store's own wording
        // is what reports it — the same message `-e` and the conformance
        // corpus already get for the same input.
        match store.run(&pending) {
            Ok(answers) => {
                for answer in &answers {
                    report(out, answer, shape)?;
                }
            }
            Err(refusal) => {
                writeln!(out, "error: {refusal}")?;
                ended = Ended::Refused;
            }
        }
    } else if remainder.substantial {
        writeln!(out, "error: input ended inside an unfinished statement")?;
        ended = Ended::Refused;
    }
    Ok(ended)
}

/// What one statement answered.
///
/// The same [`Answer`] whether the store is in this process or across a socket,
/// which is what makes the two renderings identical by construction rather than
/// by two code paths agreeing.
fn report(out: &mut impl Write, answer: &Answer, shape: Shape) -> std::io::Result<()> {
    match answer {
        Answer::Records { records, .. } if records.is_empty() => writeln!(out, "(no records)"),
        // `only` is read and deliberately not drawn. The console already prints
        // a record per line, so a read of one already looks like one record —
        // the flag exists for a caller assembling a value, and the surface where
        // it shows is the wire and the JSON, not this one. Named rather than
        // swept up by `..` so that the next field added here has to be decided
        // about instead of ignored.
        Answer::Records {
            records,
            path,
            names,
            notes,
            only: _,
            exact,
            suggestion,
        } => {
            // The trailer is the same either way, and deliberately so: how many
            // and by which path is the part an operator reads for the answer
            // behind the answer, and it should not move when the drawing does.
            match shape.drawn(records, names) {
                Some(drawn) => write!(out, "{drawn}")?,
                None => {
                    for (id, held) in records {
                        writeln!(out, "{}", render::record(id, held, names))?;
                    }
                }
            }
            // Silent when the answer is exact, which is nearly every read: a
            // word printed on every line is a word nobody reads by the third
            // one, and the trailer's job is to be worth reading. The two cases
            // that carry something are both said — including the one the note
            // channel cannot express at all, a node that never stated it.
            let exactness = match exact {
                Some(Exact::Yes) => "",
                // The reason is not repeated here. It arrives on the next line
                // as a note, and the trailer names the fact rather than
                // explaining it twice in two shapes.
                Some(Exact::No { .. }) => ", approximate",
                None => ", exactness not stated",
            };
            writeln!(out, "({} record(s), via {path}{exactness})", records.len())?;
            // After the trailer, because a note is about the answer above it.
            // One line each and none at all for almost every read, which is what
            // makes a note worth reading when one appears.
            for note in notes {
                writeln!(out, "note: {}", note.message)?;
            }
            // Silent for both of the states that have nothing to offer, on the
            // same footing as an exact answer printing no word about exactness.
            // The console prints the correction and does NOT re-run anything
            // with it: what a reader does with "did you mean" is a decision only
            // the reader can take, and a shell that quietly answers a different
            // question is the failure this whole field exists to prevent.
            if let Some(Suggested::DidYouMean(corrections)) = suggestion {
                for correction in corrections {
                    writeln!(
                        out,
                        "did you mean: {} -> {}",
                        correction.typed, correction.instead
                    )?;
                }
            }
            Ok(())
        }
        Answer::Value { value, names } => writeln!(out, "{}", render::value(value, names)),
        // A write the store named the record for answers with the identity it
        // produced, because that is the only way back to the record: the caller
        // did not choose it, cannot derive it, and has no second statement that
        // would find it. `ok` was the answer until a write existed that the
        // caller could not address afterwards, and it discarded the one thing
        // such a write owes.
        //
        // One line each rather than a list on one, so the common answer — a
        // single `CREATE` — is a single identity a person can select, and a
        // batch `INSERT` is one per row rather than something to split up.
        Answer::Keys(keys) => {
            for key in keys {
                writeln!(out, "{key}")?;
            }
            Ok(())
        }
        // `Removed` keeps the rendering it had, so the parity test certifies
        // today's output rather than an output changed in the same commit that
        // gave it a second path. Q-2026-08-22-52.
        _ => writeln!(out, "ok"),
    }
}

/// Whether the accumulated text closes a statement.
///
/// A `;` inside a string does not, which is the whole reason this is a walk
/// rather than a `contains`.
///
/// The session itself no longer asks this — it feeds a [`Scanner`] line by line
/// instead — but the tests below assert the single-pass answer, which is the
/// behaviour the incremental scan has to reproduce.
#[cfg(test)]
fn closed(text: &str) -> bool {
    scan(text).closed
}

/// What one pass over partial input found.
struct Scan {
    /// A statement ended in this text.
    closed: bool,
    /// There is something here besides whitespace and comments.
    ///
    /// The difference matters only at end of input, where text that never
    /// closed is reported as a statement that ran out. A file whose last line
    /// is a note has nothing unfinished in it.
    substantial: bool,
    /// A `BEGIN` in this text has no `COMMIT` or `CANCEL` after it.
    open_transaction: bool,
}

/// The three words that move a transaction boundary, recognised only where a
/// statement begins — so a field called `begin` is a field.
fn boundary(word: &str, open: &mut bool) {
    if word.eq_ignore_ascii_case("BEGIN") {
        *open = true;
    } else if word.eq_ignore_ascii_case("COMMIT") || word.eq_ignore_ascii_case("CANCEL") {
        *open = false;
    }
}

/// Reads far enough to find a statement's end, honouring quotes and comments.
///
/// Comments run from `--` to the end of their line, which the lexer already
/// knows and this did not. Both halves of that omission were wrong, and the
/// second one silently:
///
/// - a script ending in a comment left text that never closed, and was reported
///   as input that ran out mid-statement — sending the reader to look for an
///   unbalanced quote that is not there;
/// - a `;` **inside** a comment ended the statement early. `SELECT * FROM users
///   -- oops; a note` then `WHERE name = 'ada';` split into a read with no
///   filter and a fragment beginning `WHERE`. The first ran and printed every
///   record. Nothing reported a fault about the answer, because as far as
///   everything below here was concerned there was no fault: the wrong question
///   was asked correctly.
///
/// Kept as a single-pass entry point for the tests below: they state the
/// behaviour in terms of a whole text, and the incremental scan the session
/// runs has to agree with it however the text is cut up.
#[cfg(test)]
fn scan(text: &str) -> Scan {
    let mut scanner = Scanner::default();
    scanner.feed(text);
    scanner.state()
}

/// A scan that can be handed the next piece of input instead of the whole
/// buffer again.
///
/// # Why this is not one function over the accumulated text
///
/// It was, and outside a transaction that is free: the walk stops at the first
/// `;`, so the text it examines is one statement however long the session runs.
/// Inside a transaction the walk deliberately does **not** stop at `;` — a `;`
/// there ends a statement and not the group that has to be submitted together —
/// so the accumulated block is what gets examined, and examining it again after
/// every appended line is quadratic in the number of statements.
///
/// Measured before this existed, loading `CREATE`s through the console: 76 µs
/// per statement at 500 of them inside one transaction, 589 µs at 8 000, the
/// cost doubling each time the count did. The same statements through the node's
/// HTTP surface, which does not go through here, cost 15-23 µs and got *cheaper*
/// with batching — so this was the console's own cost and not the store's.
///
/// The state below is exactly the set of local variables the walk used to keep
/// between characters. Keeping them across calls is the whole change.
#[derive(Default)]
struct Scanner {
    quote: Option<char>,
    escaped: bool,
    commented: bool,
    substantial: bool,
    open_transaction: bool,
    closed: bool,
    /// A `-` that has been read while the character after it has not.
    ///
    /// Only a doubled dash opens a comment, so a `-` at the very end of a piece
    /// of input cannot be resolved until the next piece arrives. Holding it is
    /// what keeps a comment split across two lines reading as a comment.
    held_dash: bool,
    /// The first word of a statement is the only place a keyword is a keyword.
    at_statement_start: bool,
    word: String,
    /// Whether anything has been fed yet, which is what `at_statement_start`
    /// means before the first character.
    started: bool,
}

impl Scanner {
    /// What the scan has found so far.
    ///
    /// A word still being read and a dash still being held are both answered
    /// **provisionally**: the single-pass walk resolved them at end of input,
    /// and here the input may not have ended, so they are applied to the answer
    /// without being consumed. Feeding the rest and asking again gives the same
    /// result the single pass would have.
    fn state(&self) -> Scan {
        let mut open_transaction = self.open_transaction;
        if self.at_statement_start && !self.word.is_empty() {
            boundary(&self.word, &mut open_transaction);
        }
        Scan {
            closed: self.closed,
            substantial: self.substantial || self.held_dash,
            open_transaction,
        }
    }

    /// Read the next piece of input, continuing where the last one stopped.
    ///
    /// Stops early once a statement has closed, exactly as the single-pass walk
    /// did: everything after that `;` belongs to the next statement and is not
    /// this scan's business.
    fn feed(&mut self, text: &str) {
        if !self.started {
            self.started = true;
            self.at_statement_start = true;
        }
        if self.closed {
            return;
        }
        let mut quote = self.quote;
        let mut escaped = self.escaped;
        let mut commented = self.commented;
        let mut substantial = self.substantial;
        let mut open_transaction = self.open_transaction;
        let mut closed = false;
        let mut at_statement_start = self.at_statement_start;
        let mut word = core::mem::take(&mut self.word);
        let mut characters = text.chars().peekable();
        // A dash held from the previous piece is resolved against this one's
        // first character before anything else looks at it.
        if self.held_dash {
            self.held_dash = false;
            if characters.peek() == Some(&'-') {
                let _ = characters.next();
                commented = true;
            } else {
                substantial = true;
            }
        }
        while let Some(character) = characters.next() {
            // A word ends at anything that cannot be inside one.
            if !character.is_alphanumeric() && character != '_' && !word.is_empty() {
                if at_statement_start {
                    boundary(&word, &mut open_transaction);
                    at_statement_start = false;
                }
                word.clear();
            }

            if commented {
                commented = character != '\n';
                continue;
            }
            if escaped {
                escaped = false;
                continue;
            }
            match (quote, character) {
                (Some(_), '\\') => escaped = true,
                (Some(open), held) if held == open => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"') => {
                    substantial = true;
                    at_statement_start = false;
                    quote = Some(character);
                }
                // Only a doubled dash opens a comment. A single one is arithmetic,
                // and `-1` is a number — the lexer draws the same line.
                (None, '-') if characters.peek() == Some(&'-') => {
                    let _ = characters.next();
                    commented = true;
                }
                // A `-` at the very end of this piece cannot be judged yet: whether
                // it opens a comment depends on a character that has not arrived.
                (None, '-') if characters.peek().is_none() => self.held_dash = true,
                (None, ';') => {
                    substantial = true;
                    at_statement_start = true;
                    // A `;` inside a transaction ends a statement and not the group
                    // that has to be submitted together. Closing here is what made a
                    // `BEGIN;` in a file arrive on its own, be discarded for ending
                    // with a transaction open, and leave every statement after it
                    // to commit by itself.
                    if !open_transaction {
                        closed = true;
                        break;
                    }
                }
                (None, held) if held.is_alphanumeric() || held == '_' => {
                    substantial = true;
                    word.push(held);
                }
                (None, held) => substantial = substantial || !held.is_whitespace(),
            }
        }
        self.quote = quote;
        self.escaped = escaped;
        self.commented = commented;
        self.substantial = substantial;
        self.open_transaction = open_transaction;
        self.closed = closed;
        self.at_statement_start = at_statement_start;
        // The word is NOT terminated here — it may continue into the next piece.
        // Its effect on a transaction boundary is applied provisionally by
        // [`Scanner::state`] instead, which is what the single-pass walk did at
        // end of input.
        self.word = word;
    }
}

/// The statement a shorthand stands for, or `None` where it is not one.
///
/// Every one of these is a statement anybody can type, and `.help` prints the
/// statement beside the shorthand so that using one teaches the language rather
/// than hiding it. That is the line this file will not cross: a shorthand saves
/// keystrokes, and never reaches anything a statement could not.
/// Make text safe to stand inside a single-quoted literal.
///
/// The same two characters `bootstrap.rs` escapes, and for the same reason: a
/// passphrase holding a quote would otherwise end the literal and the rest of it
/// would be parsed as statement text. A person choosing a passphrase is exactly
/// the person most likely to put a quote in one.
fn quoted(text: &str) -> String {
    text.replace('\\', r"\\").replace('\'', r"\'")
}

fn shorthand(command: &str) -> Option<String> {
    let (word, named) = command.split_once(' ').unwrap_or((command, ""));
    let named = named.trim();
    Some(match (word, named.is_empty()) {
        (".ns", true) => "INFO FOR STORE;".to_owned(),
        (".db", true) => "INFO FOR NAMESPACE;".to_owned(),
        (".tables", true) => "INFO FOR DATABASE;".to_owned(),
        (".node", true) => "INFO FOR NODE;".to_owned(),
        (".d", false) => format!("INFO FOR TABLE {named};"),
        (".users", true) => "INFO FOR USERS;".to_owned(),
        (".user", false) => format!("INFO FOR USER {named};"),
        (".vault", false) => format!("INFO FOR VAULT {named};"),
        (".seal", true) => "SEAL VAULT;".to_owned(),
        _ => return None,
    })
}

const HELP: &str = "\
statements end with `;` and may span lines
  .help   this
  .mode   how records are drawn: auto (the default), table, document
  .timing print how long each script took
  .unseal ask for the vault passphrase without showing it, and unseal this
          session — it is drawn as dots and is not remembered, unlike the
          statement typed in full
  .exit   leave (so does Ctrl-D on an empty line)

shorthands — each runs the statement beside it, and nothing a statement cannot:
  .ns             INFO FOR STORE;
  .db             INFO FOR NAMESPACE;
  .tables         INFO FOR DATABASE;
  .d <table>      INFO FOR TABLE <table>;
  .users          INFO FOR USERS;
  .user <name>    INFO FOR USER <name>;
  .vault <name>   INFO FOR VAULT <name>;
  .seal           SEAL VAULT;
  .node           INFO FOR NODE;

editing:  ← → Home End Delete, and Ctrl-A E B F K U W L
history:  ↑ ↓ (this session only, never written to disk — statements carry
          passwords, and a history file is how one reaches a backup)
Ctrl-C    throw away the statement being typed; the session stays";

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::io::Cursor;

    use tessaridb::{Db, Parameters};

    use super::{
        Ended, Given, HELP, Lines, Mode, Piped, Scanner, Write, closed, run, scan, shorthand,
    };
    use crate::store::Embedded;

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

    #[test]
    fn a_write_the_store_named_answers_with_the_identity_it_produced() {
        // `ok` was the answer here until this wave, and it threw away the only
        // route back to the record: the caller did not choose the identity and
        // has no statement that would find it again.
        assert_eq!(said("CREATE users = { name: 'ada' };"), ["1"]);
    }

    #[test]
    fn a_batch_insert_answers_with_one_identity_per_row() {
        assert_eq!(
            said("INSERT INTO users (name) VALUES ('ada'), ('grace'), ('alan');"),
            ["1", "2", "3"]
        );
    }

    #[test]
    fn the_identity_a_uuid_table_answers_with_is_one_the_grammar_reads() {
        // The case the integer default hides, and the one this wave is for.
        // Before it, a uuid table answered with thirty-two undivided hex digits
        // — which the grammar does not read as an identity at all, so pasting
        // the answer back produced "not a duration this store can hold", a
        // refusal naming nothing a reader could act on.
        //
        // That the spelling then *finds* the record is asserted where the store
        // outlives the statement: `tessari-ql`'s `identity_spelling` parses it
        // back for all four kinds, and `tessari-session`'s `store_named_records`
        // reads the record at it. Split that way because this harness gives each
        // script its own database, so a second script here could only address a
        // record the first one did not write.
        let produced = said("CREATE sessions = { token: 'abc' };");
        let [identity] = produced.as_slice() else {
            panic!("expected one identity, got {produced:?}");
        };
        assert!(
            identity.starts_with("uuid '") && identity.ends_with('\''),
            "a uuid table should answer in the spelling the grammar reads: {identity}"
        );
        assert_eq!(
            identity.len(),
            "uuid '".len() + 36 + 1,
            "the canonical 8-4-4-4-12 form, not the undivided digits: {identity}"
        );
    }

    #[test]
    fn an_addressed_write_still_answers_the_way_it_did() {
        // The relaxation is about the write that has no identity to report. A
        // caller who supplied one is being told nothing new by hearing it back.
        assert_eq!(
            said("CREATE users:9 = { name: 'ada' };"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_semicolon_inside_a_string_does_not_end_a_statement() {
        // The corpus splitter's own test, applied to the CLI's copy of the rule.
        assert!(!closed("SET k:1 = 'a;b'"));
        assert!(closed("SET k:1 = 'a;b';"));
        assert!(!closed("CREATE users:1 = {"));
        assert!(closed("CREATE users:1 = { name: 'ada' };"));
    }

    #[test]
    fn an_escaped_quote_does_not_close_the_string_it_is_in() {
        assert!(!closed("SET k:1 = 'it\\'s;'"));
        assert!(closed("SET k:1 = 'it\\'s;';"));
    }

    #[test]
    fn a_semicolon_inside_a_comment_does_not_end_a_statement() {
        // The one that returns a **wrong answer** rather than an error. Split
        // at the semicolon in the comment, the first half is `SELECT * FROM
        // users` — which parses, runs, and prints every record, because the
        // `WHERE` that was going to narrow it is now the start of the next
        // statement. The reader sees a plausible answer to a question they did
        // not ask, and then an error about `WHERE` that describes none of it.
        assert!(!closed("SELECT * FROM users -- oops; a note\n"));
        assert!(closed(
            "SELECT * FROM users -- oops; a note\nWHERE name = 'ada';"
        ));

        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada' };\n\
             CREATE users:2 = { name: 'grace' };\n\
             SELECT * FROM users -- oops; a note\n\
             WHERE name = 'ada';\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Fine, "{out}");
        assert!(
            out.contains("(1 record(s)"),
            "the filter was dropped\n{out}"
        );
        assert!(!out.contains("grace"), "the filter was dropped\n{out}");
    }

    #[test]
    fn a_transaction_read_from_a_file_is_one_transaction() {
        // The defect this pins was silent and total: split at every `;`, a
        // `BEGIN;` arrived as a script of its own, was discarded for ending
        // with a transaction open, and every statement that followed ran as
        // its own committed write. A migration in a file was therefore not a
        // migration — it was its statements, applied one at a time, which is
        // the half-applied state the boundary exists to prevent. It worked
        // through `-e`, because that path hands the whole string over at once,
        // so the two ways of running the same script disagreed.
        assert!(!closed("BEGIN;\n"));
        assert!(!closed("BEGIN;\nCREATE users:1 = { name: 'ada' };\n"));
        assert!(closed("BEGIN;\nCREATE users:1 = { name: 'ada' };\nCOMMIT;"));
        assert!(closed("BEGIN;\nCREATE users:1 = { name: 'ada' };\nCANCEL;"));

        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             BEGIN;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada' };\n\
             COMMIT;\n\
             SELECT * FROM users;\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Fine, "{out}");
        assert!(out.contains("(1 record(s)"), "{out}");
    }

    #[test]
    fn a_cancelled_transaction_from_a_file_leaves_nothing() {
        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             BEGIN;\n\
             DEFINE COLLECTION accounts;\n\
             CREATE accounts:1 = { balance: 10 };\n\
             CANCEL;\n\
             SELECT * FROM accounts;\n",
            Mode::Script,
        );
        // The table was never defined, so the read is refused — which is the
        // corpus's assertion, and it can only hold if the `CANCEL` undid a
        // `DEFINE` that was inside the boundary with it.
        //
        // Naming the refusal is what makes this test discriminating. Before the
        // fix it also ended `Refused`, but for an unrelated reason: the `CANCEL`
        // arrived with nothing open and was itself the refusal, while the
        // `DEFINE` it was meant to undo had already committed on its own.
        assert_eq!(ended, Ended::Refused, "{out}");
        assert!(out.contains(r#"no table named "accounts""#), "{out}");
        assert!(!out.contains("balance"), "{out}");
    }

    #[test]
    fn a_transaction_left_open_is_reported_by_the_store_that_discarded_it() {
        // Not by a message this module invents. The store is the thing that
        // discarded the work, so the store's own wording is what says so, and
        // it is the same wording `-e` and the corpus already get.
        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION users;\n\
             BEGIN;\n\
             CREATE users:2 = { name: 'grace' };\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Refused, "{out}");
        assert!(out.contains("transaction still open"), "{out}");
    }

    #[test]
    fn a_field_called_begin_does_not_open_one() {
        // The keyword is only a keyword where a statement starts. A record with
        // a `begin` field is ordinary data, and reading it as an open boundary
        // would swallow every statement after it up to the end of the input.
        assert!(closed("CREATE meetings:1 = { begin: '09:00' };"));
        assert!(closed("SELECT begin FROM meetings;"));
    }

    #[test]
    fn a_script_may_end_with_a_comment() {
        // A file whose last line is a note is an ordinary file, and reporting
        // it as input that ran out mid-statement sends somebody looking for an
        // unbalanced quote that is not there.
        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n-- and that is all\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Fine, "{out}");
        assert!(!out.contains("error:"), "{out}");
    }

    #[test]
    fn feeding_the_text_in_pieces_answers_what_one_pass_over_it_answers() {
        // The property the incremental scan exists to preserve, asserted
        // directly rather than inferred from the session tests passing. The
        // session feeds one line at a time; this feeds EVERY cut of the text,
        // including cuts that land between the two dashes of a comment and
        // inside a quoted string, because those are the places a resumed scan
        // can disagree with a single pass and nothing else would notice.
        let texts = [
            "SELECT * FROM users;",
            "SET k:1 = 'a;b';",
            "SET k:1 = 'it\\'s;';",
            "SELECT * FROM users -- oops; a note\nWHERE name = 'ada';",
            "-- a whole line of comment\n",
            "BEGIN; CREATE a:1 = { n: 1 }; COMMIT;",
            "BEGIN; CREATE a:1 = { n: 1 };",
            "BEGIN",
            "COMMIT",
            "SELECT 1 - 1;",
            "SELECT 1 -- 1;\n;",
            "CREATE users:1 = {",
            "",
            "   \n  \n",
            "SELECT * FROM t WHERE s = \"a;b\";",
        ];
        for text in texts {
            let once = scan(text);
            for cut in 0..=text.len() {
                if !text.is_char_boundary(cut) {
                    continue;
                }
                let mut scanner = Scanner::default();
                scanner.feed(&text[..cut]);
                scanner.feed(&text[cut..]);
                let piecewise = scanner.state();
                assert_eq!(
                    (
                        piecewise.closed,
                        piecewise.substantial,
                        piecewise.open_transaction
                    ),
                    (once.closed, once.substantial, once.open_transaction),
                    "{text:?} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn a_comment_does_not_hide_an_unfinished_statement() {
        // The half that must keep working: text that really did run out
        // mid-statement is still reported, comment or no comment.
        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\nSELECT * FROM users\n-- and then nothing\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Refused, "{out}");
        assert!(out.contains("unfinished statement"), "{out}");
    }

    #[test]
    fn a_statement_may_span_lines() {
        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = {\n  name: 'ada'\n};\n\
             SELECT * FROM users:1;\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Fine, "{out}");
        assert!(out.contains("name: 'ada'"), "{out}");
    }

    /// A store with one flat record in it, ready to be selected from.
    const ONE_RECORD: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE COLLECTION users;\n\
CREATE users:1 = { name: 'ada' };\n\
SELECT * FROM users:1;\n";

    #[test]
    fn a_prompt_draws_a_table_and_a_script_draws_what_pastes_back() {
        // The property being protected is not the table. It is that a piped run
        // still prints TessariQL, because that is what this program promises its
        // output is — and a table is not something a statement can be fed.
        let (at_prompt, _) = ran(ONE_RECORD, Mode::Interactive);
        let (in_script, _) = ran(ONE_RECORD, Mode::Script);

        assert!(
            at_prompt.contains("| name"),
            "no table at a prompt:\n{at_prompt}"
        );
        assert!(
            in_script.contains("name: 'ada'"),
            "a script lost the pasteable form:\n{in_script}"
        );
        assert!(
            !in_script.contains("| name"),
            "a script drew a table:\n{in_script}"
        );
    }

    #[test]
    fn dot_mode_turns_the_table_off_and_reports_what_it_is() {
        let (asked, _) = ran(
            &format!(".mode document\n{ONE_RECORD}.mode\n"),
            Mode::Interactive,
        );
        assert!(
            !asked.contains("| name"),
            "`.mode document` still drew a table:\n{asked}"
        );
        assert!(asked.contains("name: 'ada'"), "{asked}");
        assert!(
            asked.lines().any(|line| line.ends_with("> document")),
            "`.mode` did not say which mode it is in:\n{asked}"
        );

        let (refused, _) = ran(".mode sideways\n", Mode::Interactive);
        assert!(refused.contains("no such mode: sideways"), "{refused}");
    }

    #[test]
    fn dot_unseal_reads_the_passphrase_from_the_next_line_and_never_echoes_it() {
        // Through `Piped`, whose `secret` is the ordinary `next` — there is no
        // terminal echoing anything into a pipe, and the masking that matters is
        // `Edited`'s. What this asserts is the half a pipe *can* show: the
        // passphrase becomes a statement, and the program never prints it back.
        let (out, _) = ran(
            &format!(
                "{READY}.unseal\nan operator passphrase\nDEFINE VAULT team;\n\
                 DEFINE FIELD token ON team TYPE string SECRET;\n\
                 CREATE team:'github' = {{ token: 'hunter2' }};\n\
                 REVEAL token FROM team:'github';\n"
            ),
            Mode::Script,
        );

        assert!(
            !out.contains("an operator passphrase"),
            "the passphrase was printed back:\n{out}"
        );
        // The control: the flow actually worked, so the assertion above is not
        // passing because nothing happened.
        assert!(out.contains("hunter2"), "the reveal did not run:\n{out}");
    }

    #[test]
    fn a_shorthand_runs_the_statement_the_help_says_it_runs() {
        // The obligation runs this way round on purpose: `.help` is the contract
        // and the table has to satisfy it. Reading the table and checking the
        // help mentions each entry would pass while the help promised a seventh
        // shorthand nobody implemented.
        let promised: Vec<(&str, &str)> = HELP
            .lines()
            .skip_while(|line| !line.starts_with("shorthands"))
            .filter_map(|line| line.trim().split_once("  "))
            .map(|(left, right)| (left.trim(), right.trim()))
            .filter(|(left, _)| left.starts_with('.'))
            .collect();
        assert_eq!(promised.len(), 9, "the help lists {promised:?}");

        for (spelling, statement) in promised {
            // `.d <table>` in the help is `.d users` at a prompt.
            let typed = spelling
                .replace("<table>", "users")
                .replace("<name>", "ada");
            let expected = statement
                .replace("<table>", "users")
                .replace("<name>", "ada");
            assert_eq!(
                shorthand(&typed).as_deref(),
                Some(expected.as_str()),
                "`{typed}` does not run what the help says it runs"
            );
        }
    }

    #[test]
    fn a_shorthand_that_needs_a_name_and_is_given_none_is_not_a_shorthand() {
        assert!(shorthand(".d").is_none());
        assert!(shorthand(".user").is_none());
        // And one that takes none refuses a name rather than ignoring it.
        assert!(shorthand(".tables users").is_none());
        assert!(shorthand(".users ada").is_none());
        // The plural and the singular are separate words to the
        // splitter, so neither can be reached by mistyping the other.
        assert_eq!(shorthand(".users").as_deref(), Some("INFO FOR USERS;"));
    }

    #[test]
    fn timing_is_off_until_it_is_asked_for() {
        let (quiet, _) = ran("DEFINE NAMESPACE prod;\n", Mode::Interactive);
        assert!(!quiet.contains("time:"), "{quiet}");

        let (timed, _) = ran(".timing\nDEFINE NAMESPACE prod;\n", Mode::Interactive);
        assert!(timed.contains("timing is on"), "{timed}");
        assert!(timed.contains("time:"), "{timed}");
    }

    #[test]
    fn a_refusal_stops_a_script_and_not_a_prompt() {
        let bad = "SELECT * FROM;\nDEFINE NAMESPACE prod;\n";
        let (script, ended) = ran(bad, Mode::Script);
        assert_eq!(ended, Ended::Refused);
        assert!(script.contains("error:"), "{script}");

        let (prompt, ended) = ran(bad, Mode::Interactive);
        assert_eq!(ended, Ended::Refused, "a refusal still sets the exit code");
        // The second statement ran, which is the difference.
        assert!(prompt.matches("tessaridb>").count() >= 2, "{prompt}");
        assert!(prompt.contains("ok"), "{prompt}");
    }

    #[test]
    fn input_ending_mid_statement_is_reported_rather_than_discarded() {
        // Silently dropping it looks exactly like a statement that ran and
        // answered nothing.
        let (out, ended) = ran("DEFINE NAMESPACE prod", Mode::Script);
        assert_eq!(ended, Ended::Refused);
        assert!(out.contains("unfinished"), "{out}");
    }

    #[test]
    fn a_dot_command_is_only_a_command_at_the_start_of_a_statement() {
        let (out, _) = ran(".help\n.nonesuch\n", Mode::Interactive);
        assert!(out.contains("statements end with"), "{out}");
        assert!(out.contains("no such command"), "{out}");
    }

    #[test]
    fn a_prompt_is_written_only_when_somebody_is_there_to_read_it() {
        let (script, _) = ran("DEFINE NAMESPACE prod;\n", Mode::Script);
        assert!(!script.contains("tessaridb>"), "{script}");
        let (prompt, _) = ran("DEFINE NAMESPACE prod;\n", Mode::Interactive);
        assert!(prompt.contains("tessaridb>"), "{prompt}");
    }

    #[test]
    fn a_read_says_how_many_and_by_which_path() {
        let (out, ended) = ran(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada' };\n\
             SELECT * FROM users;\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Fine, "{out}");
        assert!(out.contains("1 record(s), via scan"), "{out}");
    }
}
