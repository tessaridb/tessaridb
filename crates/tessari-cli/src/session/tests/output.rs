use super::*;

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
