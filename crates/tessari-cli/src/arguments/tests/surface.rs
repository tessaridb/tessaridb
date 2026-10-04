use super::*;

#[test]
fn no_arguments_is_an_in_memory_store_read_from_standard_input() {
    let held = asked(&[]).expect("defaults");
    assert!(held.store.is_none());
    assert!(matches!(held.source, Source::Standard));
}

#[test]
fn asking_for_the_version_is_its_own_thing_to_do() {
    assert!(matches!(
        asked(&["--version"]).expect("--version").source,
        Source::Version
    ));
    assert!(matches!(
        asked(&["-V"]).expect("-V").source,
        Source::Version
    ));
}

#[test]
fn asking_for_the_help_is_a_request_and_not_a_refusal() {
    // It used to be `Err(USAGE)`, which `main` printed to standard error
    // with a non-zero status — so `tessaridb --help | grep serve` came back
    // empty and any packaging check that runs `--help` failed. A parse
    // *error* is still an error; being asked for the usage is not one.
    assert!(matches!(
        asked(&["--help"]).expect("--help").source,
        Source::Help
    ));
    assert!(matches!(asked(&["-h"]).expect("-h").source, Source::Help));
}

#[test]
fn a_misspelled_flag_is_still_a_refusal_and_still_carries_the_usage() {
    // The other half of the split above: the usage text did double duty as
    // both the answer to `--help` and the body of a parse error, and only
    // the first of those changed channel.
    let complaint = asked(&["--nonsense"]).expect_err("a typo");
    assert!(
        complaint.contains("--nonsense") && complaint.contains("usage:"),
        "a refusal that names neither the flag nor the usage: {complaint}"
    );
}

#[test]
fn a_misspelled_flag_beside_the_version_is_still_reported() {
    // The reason `--version` is read by the parser rather than
    // short-circuited ahead of it. A binary that answered the version and
    // swallowed a typo would be the one place this module lets an
    // unrecognised option through.
    let complaint = asked(&["--version", "--serv", "127.0.0.1:0"]).expect_err("a typo");
    assert!(
        complaint.contains("--serv"),
        "the typo is not named: {complaint}"
    );
}

#[test]
fn a_parameter_bound_for_a_version_that_runs_no_script_is_refused() {
    let complaint = asked(&["--version", "--param", "x=1"]).expect_err("no script");
    assert!(
        complaint.contains("--version"),
        "the refusal does not say which flag runs no script: {complaint}"
    );
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
        asked(&["-f", "setup.tessariql"]).expect("a file").source,
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

#[test]
fn a_path_and_an_address_are_two_stores_and_are_refused_as_one() {
    // Quietly preferring one of them is how somebody writes to the wrong
    // store, which is the same reason two paths are refused.
    assert!(asked(&["./data", "--at", "127.0.0.1:7654"]).is_err());
    assert!(asked(&["--at", "127.0.0.1:7654", "./data"]).is_err());
    assert!(asked(&["--at", "127.0.0.1:7654"]).is_ok());
    assert!(asked(&["--at"]).is_err());
}

#[test]
fn what_reaches_past_the_session_is_refused_over_an_address() {
    // The protocol carries scripts. Ignoring the address and running these
    // against a store in this process is the opposite of what was asked.
    for reaching in [
        vec!["--at", "127.0.0.1:7654", "--health"],
        vec!["--at", "127.0.0.1:7654", "--backup", "out"],
        vec!["--at", "127.0.0.1:7654", "--restore", "in"],
        vec!["--at", "127.0.0.1:7654", "--serve", "0.0.0.0:1"],
    ] {
        let complaint = asked(&reaching).expect_err("refused");
        assert!(
            complaint.contains("--at names one it did not"),
            "{complaint}"
        );
    }
    // A script is not, because a script is what the protocol carries.
    assert!(asked(&["--at", "127.0.0.1:7654", "-e", "SELECT 1;"]).is_ok());
}

#[test]
fn a_password_is_never_read_from_an_argument() {
    // It would be in the process table for anybody on the machine and in the
    // shell history afterwards. `--password` is not a flag, so it is refused
    // the way any unknown option is.
    assert!(asked(&["--password", "hunter2"]).is_err());
    let held = asked(&["--user", "ada"]).expect("a name");
    assert_eq!(held.user.as_deref(), Some("ada"));
}

#[test]
fn a_name_without_the_password_in_the_environment_is_refused() {
    // Signing in anonymously after a failed sign-in would answer a different
    // question than the one that was asked.
    let complaint = credentials(Some("ada".to_owned()));
    match std::env::var(PASSWORD) {
        Ok(_) => assert!(complaint.is_ok()),
        Err(_) => assert!(
            complaint.expect_err("refused").contains(PASSWORD),
            "the refusal should say where the password comes from"
        ),
    }
}

#[test]
fn serving_takes_an_address_of_its_own() {
    let held = asked(&["./data", "--serve", "0.0.0.0:7654"]).expect("an address");
    assert!(matches!(held.source, Source::Serve));
    assert_eq!(held.serving.wire.as_deref(), Some("0.0.0.0:7654"));
    assert_eq!(held.serving.http, None);
    assert!(asked(&["--serve"]).is_err());
}

#[test]
fn each_surface_takes_its_own_address_and_they_may_be_asked_for_together() {
    let held = asked(&[
        "./data",
        "--serve",
        "0.0.0.0:7654",
        "--http",
        "127.0.0.1:8000",
    ])
    .expect("two addresses");
    assert!(matches!(held.source, Source::Serve));
    assert_eq!(held.serving.wire.as_deref(), Some("0.0.0.0:7654"));
    assert_eq!(held.serving.http.as_deref(), Some("127.0.0.1:8000"));

    // Either alone, because a process holds whichever subset was named.
    let only_http = asked(&["./data", "--http", "127.0.0.1:8000"]).expect("one address");
    assert_eq!(only_http.serving.wire, None);
    assert_eq!(only_http.serving.http.as_deref(), Some("127.0.0.1:8000"));

    assert!(asked(&["--http"]).is_err());
}

#[test]
fn a_backup_folder_belongs_to_a_serving_node() {
    let held = asked(&[
        "./data",
        "--http",
        "127.0.0.1:8000",
        "--backup-dir",
        "/backups",
    ])
    .expect("a folder beside a surface");
    assert_eq!(
        held.serving.backups.as_deref(),
        Some(std::path::Path::new("/backups"))
    );
    assert_eq!(
        asked(&["./data", "--http", "127.0.0.1:8000"])
            .expect("no folder")
            .serving
            .backups,
        None
    );

    // Refused rather than ignored: a folder named and never written to is
    // one somebody believes holds their backups.
    let refusal = asked(&["./data", "--backup-dir", "/backups", "-e", "SELECT 1;"])
        .expect_err("nothing serves");
    assert!(
        refusal.contains("--backup-dir"),
        "the refusal does not name the flag: {refusal}"
    );
    assert!(asked(&["./data", "--http", "127.0.0.1:8000", "--backup-dir"]).is_err());
}

#[test]
fn a_retained_count_is_a_positive_number_or_none() {
    assert_eq!(
        super::super::retained_records("100000"),
        Ok(tessari_storage::Retention::Keep(
            tessari_types::Sequence::new(100_000)
        ))
    );
    assert_eq!(
        super::super::retained_records("NONE"),
        Ok(tessari_storage::Retention::Unbounded)
    );
    for written in ["0", "-1", "ten", "", "1e5"] {
        assert!(
            super::super::retained_records(written).is_err(),
            "`{written}` is not a count, and a start that guessed would \
                 prune where nobody said"
        );
    }
}

#[test]
fn an_unseal_period_is_a_positive_duration_on_a_serving_node() {
    let held = asked(&["./data", "--http", "127.0.0.1:8000", "--unseal-for", "90s"])
        .expect("a period beside a surface");
    assert_eq!(
        held.serving.unseal_for,
        Some(core::time::Duration::from_secs(90))
    );
    assert_eq!(
        asked(&["./data", "--http", "127.0.0.1:8000"])
            .expect("no period")
            .serving
            .unseal_for,
        None
    );

    for (text, why) in [
        ("0s", "zero"),
        ("ten", "not a duration"),
        ("10", "a number"),
    ] {
        let refusal =
            asked(&["./data", "--http", "127.0.0.1:8000", "--unseal-for", text]).expect_err(why);
        assert!(
            refusal.contains("--unseal-for"),
            "the refusal of {why} does not name the flag: {refusal}"
        );
    }
    let refusal =
        asked(&["./data", "--unseal-for", "10m", "-e", "SELECT 1;"]).expect_err("nothing serves");
    assert!(refusal.contains("--unseal-for"), "{refusal}");
}

#[test]
fn an_address_next_to_something_that_is_not_serving_is_refused() {
    // Not ignored. A port that was named and never opened is worse than one
    // that was refused, because nothing says which happened.
    let refusal = asked(&["./data", "--http", "127.0.0.1:8000", "-e", "SELECT 1;"])
        .expect_err("two programs");
    assert!(
        refusal.contains("two programs"),
        "the refusal should say why: {refusal}"
    );
}
