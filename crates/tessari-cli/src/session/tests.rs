#![allow(clippy::panic)]

use std::io::Cursor;

use tessaridb::{Db, Parameters};

use super::{Ended, Given, HELP, Lines, Mode, Piped, Scanner, Write, closed, run, scan, shorthand};
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
fn a_semicolon_inside_an_events_braces_does_not_end_it() {
    // `DEFINE EVENT … THEN { a; b; }` is one statement whose body holds
    // several, so the `;` inside the braces ends a body statement and not
    // the definition — typed across lines, the first one would otherwise
    // send half a definition (ADR-0110).
    let typed = "DEFINE EVENT e ON t THEN {\n    CREATE log = { v: 1 };\n";
    assert!(!closed(typed));
    assert!(!closed(&format!("{typed}    CREATE log = {{ v: 2 }};\n")));
    assert!(closed(&format!("{typed}}};")));
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
fn a_statement_begun_after_a_semicolon_may_continue_on_the_next_line() {
    // A line that closes one statement and opens another is not a closed
    // line: submitted there, the second statement went over half-written and
    // was refused as a script that ran out (Q-872). Every documentation page
    // that shows a read wrapped over two lines beside a write hit it.
    assert!(!closed(
        "CREATE notes:1 = { body: 'a' }; SELECT body AS r\n"
    ));
    assert!(closed(
        "CREATE notes:1 = { body: 'a' }; SELECT body AS r\nFROM notes;"
    ));
    // Only what follows the last `;` is judged: a note or blank space there
    // leaves nothing unfinished.
    assert!(closed("SELECT 1; -- a note\n"));
    assert!(closed("SELECT 1;   \n"));

    let (out, ended) = ran(
        &format!(
            "{READY}CREATE users:1 = {{ name: 'ada' }}; SELECT name AS r\n\
                 FROM users;\n"
        ),
        Mode::Script,
    );
    assert_eq!(ended, Ended::Fine, "{out}");
    assert!(out.contains("r: 'ada'"), "the read did not run\n{out}");
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

/// A statement that shares a line with a following `BEGIN` is answered, not
/// thrown away with the transaction it was standing next to. Q-370.
#[test]
fn a_statement_sharing_a_line_with_a_begin_is_not_discarded() {
    let (out, ended) = ran(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:2 = { n: 1 };\n\
             SELECT * FROM users:2; BEGIN;\n\
             COMMIT;\n",
        Mode::Script,
    );
    assert_eq!(ended, Ended::Fine, "{out}");
    assert!(
        out.contains("1 record(s), via record"),
        "the read beside the BEGIN never answered: {out}"
    );
    assert!(
        !out.contains("discarded"),
        "the script was thrown away: {out}"
    );
}

/// An empty answer is an answer, and it owes the reader what every other
/// answer owes: how many, by which path, and whether the store stands
/// behind it. Q-393.
#[test]
fn an_empty_answer_says_how_many_and_by_which_path() {
    let (out, ended) = ran(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada' };\n\
             SELECT * FROM users WHERE name = 'nobody';\n",
        Mode::Script,
    );
    assert_eq!(ended, Ended::Fine, "{out}");
    assert!(out.contains("0 record(s), via scan"), "{out}");
}

/// The suggestion is the whole reason the empty case matters: a reader who
/// got nothing has nothing else to go on, and the store already knows what
/// they probably meant. This is the reference's own example at
/// `docs/tessariql.md`, made runnable. Q-393.
#[test]
fn an_empty_answer_still_says_what_you_probably_meant() {
    let (out, ended) = ran(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase, ascii;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n\
             CREATE notes:1 = { body: 'a vector store' };\n\
             SELECT * FROM notes WHERE body MATCHES 'vecter';\n",
        Mode::Script,
    );
    assert_eq!(ended, Ended::Fine, "{out}");
    assert!(out.contains("0 record(s), via index"), "{out}");
    assert!(out.contains("did you mean: vecter -> vector"), "{out}");
}

/// G022 S7 on the surface a person reads: *a caller can never be unable to
/// tell which it got*. The criterion was recorded PASS against answers that
/// have records; an empty approximate answer and an empty exact one were
/// the same three words. Q-393.
#[test]
fn an_empty_approximate_answer_is_not_an_empty_exact_one() {
    const READY_POINTS: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION points;\n\
             DEFINE INDEX by_at ON points FIELDS at VECTOR euclidean;\n";
    let exactly = format!(
        "{READY_POINTS}SELECT * FROM points ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 2;\n"
    );
    let approximately = format!(
        "{READY_POINTS}SELECT * FROM points ORDER BY vector::euclidean(at, [0.0, 0.0]) LIMIT 2 APPROXIMATE;\n"
    );
    let (exact, ended) = ran(&exactly, Mode::Script);
    assert_eq!(ended, Ended::Fine, "{exact}");
    let (approximate, ended) = ran(&approximately, Mode::Script);
    assert_eq!(ended, Ended::Fine, "{approximate}");
    assert_ne!(
        exact, approximate,
        "an empty answer must still say which path answered it"
    );
    assert!(approximate.contains("approximate"), "{approximate}");
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
