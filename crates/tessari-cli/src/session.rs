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
use crate::scanner::Scanner;
#[cfg(test)]
use crate::scanner::{closed, scan};
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
            let found = scanner.state();
            // Both facts, because either alone submits the wrong thing: without
            // `closed` a half-typed statement goes over, and without the
            // transaction check a statement standing beside a `BEGIN` is sent as
            // a script that ends with one open, which the store discards whole.
            if !found.closed || found.open_transaction {
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
        // No arm of its own for the empty answer. There was one, printing
        // `(no records)`, and it swept four fields through a `..`: the trailer,
        // the notes, the exactness word and the suggestion. An empty answer is
        // the one that has nothing else to go on, so it is the one that can
        // least afford to lose them — and an empty *approximate* answer read
        // identically to an empty exact one, which is the criterion that a
        // caller can never be unable to tell which it got, failing on the
        // surface a person actually reads. `drawn` answers `None` for an empty
        // list and the record loop then prints nothing, so the arm below
        // renders both cases without a branch. Q-393.
        //
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
#[path = "session/tests.rs"]
mod tests;
