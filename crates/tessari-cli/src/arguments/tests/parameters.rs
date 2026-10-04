use super::*;

#[test]
fn a_parameter_is_read_as_a_tessariql_value() {
    let held = asked(&[
        "-e",
        "SELECT * FROM users WHERE name = $who;",
        "--param",
        "who='ada'",
    ])
    .expect("a parameter");
    assert_eq!(
        held.parameters.get("who"),
        Some(&Value::String("ada".to_owned()))
    );
}

#[test]
fn a_parameter_keeps_the_kinds_json_would_have_flattened() {
    let held = asked(&[
        "-e",
        "SELECT 1;",
        "--param",
        "cost=dec 12.34",
        "--param",
        "span=1h30m",
    ])
    .expect("two parameters");
    assert!(matches!(
        held.parameters.get("cost"),
        Some(Value::Number(Number::Decimal(_)))
    ));
    assert!(matches!(
        held.parameters.get("span"),
        Some(Value::Duration(_))
    ));
}

#[test]
fn a_parameter_may_be_written_with_its_marker() {
    // `--param $who='ada'` is what somebody types after reading the script,
    // and refusing it would be pedantry with a shell-quoting trap attached.
    let held = asked(&["-e", "SELECT 1;", "--param", "$who='ada'"]).expect("a parameter");
    assert!(held.parameters.contains_key("who"));
}

#[test]
fn a_parameter_that_is_not_a_value_is_refused() {
    // The rule one layer out from the grammar: a value is a value or it is
    // nothing, so a statement smuggled in as one is refused here rather
    // than pasted into a script.
    for given in [
        "who=1; DROP TABLE users",
        "who=SELECT * FROM users",
        "who=name",
    ] {
        assert!(
            asked(&["-e", "SELECT 1;", "--param", given]).is_err(),
            "{given} was accepted as a value"
        );
    }
}

#[test]
fn a_parameter_needs_a_name_and_a_value() {
    assert!(asked(&["--param"]).is_err());
    assert!(asked(&["--param", "who"]).is_err());
    assert!(asked(&["--param", "='ada'"]).is_err());
}

#[test]
fn a_sequence_bounds_a_backup_or_a_restore_and_nothing_else() {
    assert_eq!(
        asked(&["./data", "--backup", "./out", "--from", "42"])
            .expect("a bounded backup")
            .at_sequence,
        Some(42)
    );
    assert_eq!(
        asked(&["./data", "--restore", "./in", "--upto", "7"])
            .expect("a bounded restore")
            .at_sequence,
        Some(7)
    );
    // Elsewhere it means nothing, and silence would answer with everything.
    assert!(asked(&["./data", "--from", "42"]).is_err());
    assert!(asked(&["./data", "--health", "--upto", "7"]).is_err());
    assert!(asked(&["./data", "--backup", "./out", "--from", "soon"]).is_err());
}

#[test]
fn verifying_needs_a_path_and_no_store() {
    assert!(matches!(
        asked(&["--verify", "./held"]).expect("a path").source,
        Source::Verify(_)
    ));
    assert!(asked(&["--verify"]).is_err());
    // It reads a file, so an address is a store it was not asked about.
    assert!(asked(&["--at", "127.0.0.1:1", "--verify", "./held"]).is_err());
}

#[test]
fn a_parameter_is_refused_where_no_script_runs() {
    // Ignoring it would leave somebody believing a value was used.
    for source in [
        vec!["./data", "--health"],
        vec!["./data", "--backup", "./out"],
        vec!["./data", "--serve", "127.0.0.1:0"],
    ] {
        let mut arguments = source.clone();
        arguments.extend(["--param", "who='ada'"]);
        assert!(
            asked(&arguments).is_err(),
            "{source:?} accepted a parameter"
        );
    }
}

/// The usage names every option form the parser accepts.
///
/// # Why this reads the source and not a list
///
/// `--help` listed four forms fewer than the binary accepted — `--execute`,
/// `--file`, `-V` and `-h` all worked and none was printed — and the
/// documentation site published the complete table under the sentence
/// *"this is the binary's own usage"*, which was therefore false in one
/// direction, with the incomplete side being the binary (Q-367).
///
/// A list of forms written beside the parser would pin two lists to each
/// other and neither to the binary, so this reads the `match` itself. The
/// scan stops at the test module, so a form spelled inside a test is not
/// mistaken for one the parser takes.
#[test]
fn the_usage_names_every_option_the_parser_accepts() {
    let source = include_str!("../../arguments.rs");
    let parser = source
        .split_once("#[cfg(test)]")
        .map_or(source, |(before, _)| before);
    let mut forms = Vec::new();
    for line in parser.lines().filter(|line| line.contains("=>")) {
        let mut rest = line;
        while let Some(open) = rest.find('"') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('"') else { break };
            let (form, after) = rest.split_at(close);
            rest = &after[1..];
            if form.starts_with('-') && form.len() > 1 {
                forms.push(form.to_owned());
            }
        }
    }
    assert!(forms.len() > 10, "the scan found almost nothing: {forms:?}");
    let missing: Vec<&String> = forms
        .iter()
        .filter(|form| !USAGE.contains(form.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "the parser accepts option forms the usage does not print: {missing:?}"
    );
}
