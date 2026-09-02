//! `DEFINE CONSUMER`, `DROP CONSUMER` and `INFO FOR CONSUMER[S]` as text.
//!
//! # What this file is checking against
//!
//! ADR-0023 §2 fixes the statement's fields as a table of **nine** units, and
//! that table is a correspondence obligation rather than a summary: a clause
//! that parses into the wrong field, or one that is quietly optional, is a
//! defect no amount of "it compiles" will find.
//!
//! So each of the nine has an assertion that reads the value back out of the
//! tree, and each of the seven required ones has a refusal test as well. A
//! refusal test is not redundant with the accept: the failure being prevented is
//! a clause that is *silently* skipped, which an accepting test passes straight
//! over because the statement it wrote had the clause in it.
//!
//! # Why there is no text round trip here
//!
//! `SELECT` is the only form this milestone renders back out; every other
//! statement returns `Unrenderable` naming itself, which the renderer documents
//! as a staging order. So the round trip asserted here is the parse-level one —
//! source in, fields out — which is how every other `DEFINE` is tested today.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_ql::{InfoSubject, OnFailure, StatementKind, parse};

/// The declaration's clauses, in the order the grammar reads them.
///
/// Held as parts rather than as one string so that a test for a missing clause
/// removes exactly one — an earlier version cut by searching the whole text and
/// silently removed nothing for the last clause, which made its refusal test
/// pass a statement that was still complete.
const CLAUSES: &[(&str, &str)] = &[
    ("DEFINE CONSUMER", "DEFINE CONSUMER orders_in"),
    ("FROM", "FROM 'broker-1:9092', 'broker-2:9092'"),
    ("TOPIC", "TOPIC 'orders'"),
    ("GROUP", "GROUP 'shop-orders'"),
    ("FORMAT", "FORMAT json"),
    ("INTO", "INTO shop.orders"),
    ("IDENTITY", "IDENTITY order_id"),
    ("MAP", "MAP amount AS total, placed.at AS placed_at"),
    ("ON FAILURE", "ON FAILURE quarantine"),
    ("PARALLELISM", "PARALLELISM 2"),
];

/// The whole, well-formed declaration.
fn whole() -> String {
    without("")
}

/// The declaration with the clause led by `lead` left out.
///
/// An empty `lead` leaves everything in, which is what makes [`whole`] and the
/// variants below the same construction rather than two that can drift.
fn without(lead: &str) -> String {
    let kept: Vec<&str> = CLAUSES
        .iter()
        .filter(|(name, _)| *name != lead)
        .map(|(_, text)| *text)
        .collect();
    let expected = CLAUSES.len().saturating_sub(usize::from(!lead.is_empty()));
    assert_eq!(kept.len(), expected, "{lead:?} is not one of the clauses");
    format!("{};", kept.join(" "))
}

/// The whole declaration with `from` replaced by `to`.
fn varied(from: &str, to: &str) -> String {
    let source = whole();
    assert!(source.contains(from), "{from:?} is not in the declaration");
    source.replace(from, to)
}

/// The one statement a source parses to.
fn only(source: &str) -> StatementKind {
    let script =
        parse(source).unwrap_or_else(|failure| panic!("{source} did not parse: {failure}"));
    assert_eq!(script.statements.len(), 1, "{source} is not one statement");
    script.statements[0].kind.clone()
}

/// What refusing `source` says, or an empty string if it was accepted.
fn refusal(source: &str) -> String {
    parse(source)
        .map(|_| String::new())
        .unwrap_or_else(|error| error.to_string())
}

// ---------------------------------------------------------------- the nine units

#[test]
fn every_field_of_the_declaration_arrives_where_it_was_written() {
    // The nine units of ADR-0023 §2, asserted one at a time in one place. A test
    // per field would read better and catch less: what is being checked here is
    // that no two clauses landed in each other's field, which only a whole
    // statement can show.
    let StatementKind::DefineConsumer {
        name,
        source,
        group,
        format,
        identity,
        mapping,
        destination,
        on_failure,
        parallelism,
        if_not_exists,
    } = only(&whole())
    else {
        panic!("the whole statement did not parse as a consumer");
    };

    assert_eq!(name.text, "orders_in", "unit 1 — name");
    assert_eq!(
        source.brokers,
        vec!["broker-1:9092".to_owned(), "broker-2:9092".to_owned()],
        "unit 2 — brokers"
    );
    assert_eq!(source.topic, "orders", "unit 2 — topic");
    assert_eq!(group, "shop-orders", "unit 3 — group");
    assert_eq!(format.text, "json", "unit 4 — format");
    assert_eq!(identity.path.to_string(), "order_id", "unit 5 — identity");
    assert_eq!(mapping.len(), 2, "unit 5 — mapping");
    assert_eq!(mapping[0].from.path.to_string(), "amount");
    assert_eq!(mapping[0].to.text, "total");
    assert_eq!(
        mapping[1].from.path.to_string(),
        "placed.at",
        "a nested message field is a path, so a realistic payload is expressible"
    );
    assert_eq!(mapping[1].to.text, "placed_at");
    assert_eq!(
        destination
            .database
            .as_ref()
            .map(|named| named.text.as_str()),
        Some("shop"),
        "unit 6 — destination database"
    );
    assert_eq!(
        destination.name.text, "orders",
        "unit 6 — destination table"
    );
    assert_eq!(on_failure, OnFailure::Quarantine, "unit 7 — failure policy");
    assert_eq!(parallelism, Some(2), "unit 8 — parallelism");
    assert!(!if_not_exists, "unit 9 — if_not_exists");
}

#[test]
fn parallelism_is_the_one_clause_that_may_be_left_out() {
    // And leaving it out reads as **one**, not as "decide for me". A `None` that
    // meant "the store picks" would be a concurrency level nobody chose, which
    // is the shape of the setting whose absence this grammar is refusing.
    let StatementKind::DefineConsumer { parallelism, .. } = only(&without("PARALLELISM")) else {
        panic!("not a consumer");
    };
    assert_eq!(parallelism, None);
}

#[test]
fn if_not_exists_is_read_before_the_name() {
    let StatementKind::DefineConsumer {
        name,
        if_not_exists,
        ..
    } = only(&varied(
        "CONSUMER orders_in",
        "CONSUMER IF NOT EXISTS orders_in",
    ))
    else {
        panic!("not a consumer");
    };
    assert!(if_not_exists);
    assert_eq!(
        name.text, "orders_in",
        "the name was swallowed by the guard clause"
    );
}

// ----------------------------------------------------------------- the refusals

#[test]
fn a_missing_clause_is_refused_and_the_refusal_names_what_was_wanted() {
    // Eight clause words for the ADR's seven required units, because `source` is
    // one unit written as two clauses — `FROM` and `TOPIC` — and each of them can
    // be left out on its own.
    //
    // The message is asserted rather than `is_err()`, because an author who left
    // out `GROUP` and is told "expected a statement" has been told nothing, and
    // because a parser refusing for some *other* reason would pass an `is_err()`
    // test forever.
    for clause in [
        "FROM",
        "TOPIC",
        "GROUP",
        "FORMAT",
        "INTO",
        "IDENTITY",
        "MAP",
        "ON FAILURE",
    ] {
        let source = without(clause);
        let failure = refusal(&source);
        assert!(
            !failure.is_empty(),
            "a declaration with no {clause} clause was accepted: {source}"
        );
        assert!(
            failure.contains(clause),
            "leaving out {clause} was refused without naming it: {failure}"
        );
    }
}

#[test]
fn there_is_no_third_failure_policy_and_the_refusal_says_which_two_there_are() {
    // The refusal an operator will actually hit, because a skip mode is the
    // thing they will reach for — and the reason it does not exist has to be
    // discoverable at the point they reach.
    let failure = refusal(&varied("ON FAILURE quarantine", "ON FAILURE skip"));
    assert!(failure.contains("STOP"), "{failure}");
    assert!(failure.contains("QUARANTINE"), "{failure}");
}

#[test]
fn stop_is_the_other_policy_and_it_parses() {
    let StatementKind::DefineConsumer { on_failure, .. } =
        only(&varied("ON FAILURE quarantine", "ON FAILURE stop"))
    else {
        panic!("not a consumer");
    };
    assert_eq!(on_failure, OnFailure::Stop);
}

#[test]
fn a_parallelism_of_zero_is_refused_rather_than_read_as_one() {
    // A consumer declared to run nothing is a consumer an operator believes is
    // consuming. Refused where it is written, so nobody has to find out from an
    // empty destination table.
    assert!(parse(&varied("PARALLELISM 2", "PARALLELISM 0")).is_err());
    assert!(parse(&varied("PARALLELISM 2", "PARALLELISM -1")).is_err());
}

#[test]
fn the_brokers_are_a_list_and_one_of_them_is_enough() {
    let StatementKind::DefineConsumer { source, .. } =
        only(&varied("'broker-1:9092', 'broker-2:9092'", "'only:9092'"))
    else {
        panic!("not a consumer");
    };
    assert_eq!(source.brokers, vec!["only:9092".to_owned()]);
}

// -------------------------------------------------------- the words stay usable

#[test]
fn consumer_and_its_clause_words_are_still_available_as_names() {
    // The reason every clause word here is contextual rather than reserved, and
    // the property that reserving them would silently take away from data that
    // already exists. `consumer`, `topic`, `group`, `format` and `map` are all
    // plausible tables and fields in an application that has customers.
    for source in [
        "DEFINE COLLECTION consumer;",
        "DEFINE COLLECTION topic;",
        "DEFINE COLLECTION format;",
        "SELECT * FROM consumer;",
        "SELECT group FROM topic;",
        "CREATE map:1 = { identity: 'a', parallelism: 2, failure: 'none' };",
    ] {
        assert!(parse(source).is_ok(), "{source} stopped parsing");
    }
}

// ------------------------------------------------------------- the other two forms

#[test]
fn a_consumer_can_be_dropped_by_name() {
    let StatementKind::DropConsumer { name } = only("DROP CONSUMER orders_in;") else {
        panic!("not a drop");
    };
    assert_eq!(name.text, "orders_in");
}

#[test]
fn the_two_info_subjects_are_distinct() {
    // Singular takes a name, plural takes none. Asserted together because the
    // failure worth catching is the plural being read as the singular with a
    // missing name — which would refuse `INFO FOR CONSUMERS` with a message
    // about a name nobody meant to write.
    assert_eq!(
        only("INFO FOR CONSUMERS;"),
        StatementKind::Info {
            subject: InfoSubject::Consumers
        }
    );
    let StatementKind::Info {
        subject: InfoSubject::Consumer(name),
    } = only("INFO FOR CONSUMER orders_in;")
    else {
        panic!("not a consumer report");
    };
    assert_eq!(name.text, "orders_in");
}

#[test]
fn an_unknown_info_subject_lists_the_two_new_ones() {
    // The list in the refusal is the only place a caller finds out what they
    // may ask for, so a subject added without being listed is one nobody can
    // discover.
    let failure = refusal("INFO FOR PELICANS;");
    assert!(failure.contains("CONSUMER"), "{failure}");
    assert!(failure.contains("CONSUMERS"), "{failure}");
}

#[test]
fn the_define_refusal_lists_consumer_too() {
    let failure = refusal("DEFINE PELICAN p;");
    assert!(failure.contains("CONSUMER"), "{failure}");
}
