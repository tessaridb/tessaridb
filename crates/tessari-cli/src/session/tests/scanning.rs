use super::*;

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
