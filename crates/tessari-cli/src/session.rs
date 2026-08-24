//! Reading statements from somewhere and running them.
//!
//! # One rule, and it is the corpus's rule
//!
//! Input is accumulated until a `;` **outside a string** closes a statement.
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

/// Whether input is coming from a person or from a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A prompt: a refusal is reported and the session continues.
    Interactive,
    /// A script: a refusal stops the run.
    Script,
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
    input: &mut impl BufRead,
    out: &mut impl Write,
    mode: Mode,
) -> std::io::Result<Ended> {
    let mut pending = String::new();
    let mut ended = Ended::Fine;

    loop {
        if mode == Mode::Interactive {
            write!(
                out,
                "{}",
                if pending.is_empty() {
                    "tessaridb> "
                } else {
                    "   > "
                }
            )?;
            out.flush()?;
        }
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();

        // Dot-commands are read only at the start of a statement, so a `.exit`
        // pasted inside an unfinished object is data rather than a command.
        if pending.is_empty() && trimmed.starts_with('.') {
            match trimmed {
                ".exit" | ".quit" => break,
                ".help" => writeln!(out, "{HELP}")?,
                other => writeln!(out, "no such command: {other}\n{HELP}")?,
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
                    report(out, answer)?;
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
    if !pending.trim().is_empty() {
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
fn report(out: &mut impl Write, answer: &Answer) -> std::io::Result<()> {
    match answer {
        Answer::Records { records, .. } if records.is_empty() => writeln!(out, "(no records)"),
        Answer::Records {
            records,
            path,
            names,
        } => {
            for (id, held) in records {
                writeln!(out, "{}", render::record(id, held, names))?;
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
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some(_), '\\') => escaped = true,
            (Some(open), held) if held == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(character),
            (None, ';') => return true,
            (None, _) => {}
        }
    }
    false
}

const HELP: &str = "\
statements end with `;` and may span lines
  .help   this
  .exit   leave (so does end-of-input, Ctrl-D)

there is no line editing or history: arrow keys will print escape codes.
that is a dependency not yet taken rather than an oversight.";

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::io::Cursor;

    use tessari::{Db, Parameters};

    use super::{Ended, Mode, closed, run};
    use crate::store::Embedded;

    fn ran(script: &str, mode: Mode) -> (String, Ended) {
        let db = Db::in_memory().expect("a database");
        let mut store = Embedded::new(&db, None, Parameters::new()).expect("a session");
        let mut input = Cursor::new(script.as_bytes().to_vec());
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
