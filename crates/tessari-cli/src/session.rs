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

use tessari_wire::Answer;

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
    let mut ended = Ended::Fine;
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
                continue;
            }
            Given::Ended => break,
        };
        let trimmed = line.trim();

        // Dot-commands are read only at the start of a statement, so a `.exit`
        // pasted inside an unfinished object is data rather than a command.
        if pending.is_empty() && trimmed.starts_with('.') {
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
                    other => writeln!(out, "no such command: {other}\n{HELP}")?,
                },
            }
            continue;
        }

        pending.push_str(&line);
        if !closed(&pending) {
            continue;
        }
        let script = core::mem::take(&mut pending);
        if script.trim().is_empty() {
            continue;
        }

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
    }

    // Input that ran out mid-statement is worth saying: silently discarding it
    // looks exactly like a statement that ran and answered nothing.
    let remainder = scan(&pending);
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
        Answer::Records {
            records,
            path,
            names,
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
            writeln!(out, "({} record(s), via {path})", records.len())
        }
        Answer::Value { value, names } => writeln!(out, "{}", render::value(value, names)),
        // `Keys` and `Removed` have never had a rendering of their own and keep
        // the one they had, so the parity test certifies today's output rather
        // than an output changed in the same commit that gave it a second path.
        // Q-2026-08-22-52.
        _ => writeln!(out, "ok"),
    }
}

/// Whether the accumulated text closes a statement.
///
/// A `;` inside a string does not, which is the whole reason this is a walk
/// rather than a `contains`.
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
fn scan(text: &str) -> Scan {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut commented = false;
    let mut substantial = false;
    let mut open_transaction = false;
    let mut closed = false;
    // The first word of a statement is the only place a keyword is a keyword.
    let mut at_statement_start = true;
    let mut word = String::new();
    let mut characters = text.chars().peekable();
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
    // A word running up to the end of the text was never terminated above.
    if at_statement_start && !word.is_empty() {
        boundary(&word, &mut open_transaction);
    }
    Scan {
        closed,
        substantial,
        open_transaction,
    }
}

const HELP: &str = "\
statements end with `;` and may span lines
  .help   this
  .mode   how records are drawn: auto (the default), table, document
  .exit   leave (so does Ctrl-D on an empty line)

editing:  ← → Home End Delete, and Ctrl-A E B F K U W L
history:  ↑ ↓ (this session only, never written to disk — statements carry
          passwords, and a history file is how one reaches a backup)
Ctrl-C    throw away the statement being typed; the session stays";

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::io::Cursor;

    use tessaridb::{Db, Parameters};

    use super::{Ended, Given, Lines, Mode, Piped, Write, closed, run};
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
             DEFINE TABLE users;\n\
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
             DEFINE TABLE users;\n\
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
             DEFINE TABLE accounts;\n\
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
             DEFINE TABLE users;\n\
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
             DEFINE TABLE users;\n\
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
DEFINE TABLE users;\n\
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
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'ada' };\n\
             SELECT * FROM users;\n",
            Mode::Script,
        );
        assert_eq!(ended, Ended::Fine, "{out}");
        assert!(out.contains("1 record(s), via scan"), "{out}");
    }
}
