//! Ranking, end to end: a score in a projection and in an `ORDER BY`, and the
//! statistics it is measured against.
//!
//! These assert what the unit tests in `rank.rs` cannot: that the numbers a
//! score is computed from are the numbers the store actually maintained, across
//! writes, updates, deletes, and an index defined on a table that already had
//! rows in it. A ranking is only as honest as its statistics, and a statistic
//! that drifts produces an order that still looks like one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::{Catalog, Store};
use tessari_types::{Analyzer, DatabaseId, NamespaceId, Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A session with `prod / orders` selected, an analyzed `body` field on `notes`,
/// and a search index over it.
fn searchable(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;",
        )
        .unwrap();
    session
}

fn write(session: &mut Session<'_>, id: u64, body: &str) {
    session
        .run(&format!("CREATE notes:{id} = {{ body: '{body}' }};"))
        .unwrap();
}

fn rewrite(session: &mut Session<'_>, id: u64, body: &str) {
    session
        .run(&format!("UPDATE notes:{id} = {{ body: '{body}' }};"))
        .unwrap();
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(object) = value else {
        panic!("not an object: {value:?}");
    };
    object
        .get(name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn number(value: &Value) -> f64 {
    match value {
        Value::Number(Number::Float(held)) => *held,
        other => panic!("not a score: {other:?}"),
    }
}

/// The record ids a script answers with, in the order it answered.
fn ordered(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

#[test]
fn a_score_is_addressable_in_a_projection() {
    let store = store();
    let mut session = searchable(&store);
    write(&mut session, 1, "lock contention on the write path");
    write(&mut session, 2, "an unrelated note about breakfast");

    let outcomes = session
        .run(
            "SELECT body, search::score(body, 'lock contention') AS relevance \
             FROM notes ORDER BY relevance DESC;",
        )
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, RecordId::Int(1));
    assert!(number(field(&records[0].1, "relevance")) > 0.0);
    // The record holding none of the words scores zero — a computed answer, not
    // an absence, which is what keeps it below the matches under `DESC`.
    assert_eq!(number(field(&records[1].1, "relevance")), 0.0);
}

#[test]
fn a_score_orders_a_read_that_never_projects_it() {
    // The `ORDER BY` case has to work without the projection naming the score,
    // because a caller ranking results usually wants the records rather than the
    // number. It is also the case a resolution driven only by the `WHERE` would
    // miss: nothing else in this statement mentions `body`.
    let store = store();
    let mut session = searchable(&store);
    write(&mut session, 1, "a note mentioning locks once");
    write(&mut session, 2, "locks locks locks and more locks");

    let order = ordered(
        &mut session,
        "SELECT * FROM notes ORDER BY search::score(body, 'locks') DESC;",
    );
    assert_eq!(order, vec![RecordId::Int(2), RecordId::Int(1)]);
}

#[test]
fn a_score_can_filter_as_well_as_order() {
    let store = store();
    let mut session = searchable(&store);
    write(&mut session, 1, "lock contention");
    write(&mut session, 2, "breakfast");

    let order = ordered(
        &mut session,
        "SELECT * FROM notes WHERE search::score(body, 'lock') > 0;",
    );
    assert_eq!(order, vec![RecordId::Int(1)]);
}

#[test]
fn a_rarer_word_lifts_the_record_that_holds_it() {
    // The property that separates a ranking from a count of matches. Every
    // record here holds exactly one query word once, so the only thing that can
    // order them is how many other records hold that word.
    let store = store();
    let mut session = searchable(&store);
    for id in 1..=8 {
        write(&mut session, id, "common");
    }
    write(&mut session, 9, "rare");

    let order = ordered(
        &mut session,
        "SELECT * FROM notes WHERE body MATCHES 'common' OR body MATCHES 'rare' \
         ORDER BY search::score(body, 'common rare') DESC;",
    );
    assert_eq!(order[0], RecordId::Int(9), "the rare word did not win");
}

#[test]
fn a_score_without_a_search_index_is_refused_and_names_the_field() {
    // Not answered with zero, and not scored against whatever was read: both
    // give an order that looks like a ranking and is not one.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             CREATE notes:1 = { body: 'lock contention' };",
        )
        .unwrap();

    let refused = session
        .run("SELECT search::score(body, 'lock') AS s FROM notes;")
        .unwrap_err();
    match refused {
        Error::NoSearchIndex { field, .. } => assert_eq!(field, "body"),
        other => panic!("wrong refusal: {other}"),
    }
}

#[test]
fn the_maintained_statistics_equal_a_recount_after_writes_updates_and_deletes() {
    // The test the whole key kind exists for. A statistic is only useful while
    // it agrees with the records, and the ways it can stop agreeing are an
    // update that counts the arrival without the departure, and a delete that
    // counts neither.
    let store = store();
    let mut session = searchable(&store);

    write(&mut session, 1, "one two three");
    write(&mut session, 2, "two three four five");
    write(&mut session, 3, "six");
    // An update, which must remove the old length as well as add the new one.
    rewrite(&mut session, 2, "two");
    // A delete, which must remove a document and its whole length.
    session.run("DELETE notes:3;").unwrap();
    // A record with no text in the indexed field is in neither the postings nor
    // the count.
    session
        .run("CREATE notes:4 = { title: 'no body at all' };")
        .unwrap();
    // And one whose text analyses to nothing.
    write(&mut session, 5, "   ");

    assert_statistics_match_the_records(&store, &mut session);
}

#[test]
fn defining_an_index_on_a_populated_table_counts_what_was_already_there() {
    // The second path into the statistics, and it has to reach the same numbers
    // as the first — otherwise the same collection ranks differently depending
    // on whether the index was declared before or after the rows.
    let afterwards = store();
    let mut session = Session::new(&afterwards);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             CREATE notes:1 = { body: 'one two three' };\n\
             CREATE notes:2 = { body: 'two three' };\n\
             CREATE notes:3 = { title: 'no body' };",
        )
        .unwrap();
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    assert_statistics_match_the_records(&afterwards, &mut session);

    // And the same collection built the other way round agrees, number for
    // number — which is the claim, not merely that each is self-consistent.
    let beforehand = store();
    let mut fresh = searchable(&beforehand);
    write(&mut fresh, 1, "one two three");
    write(&mut fresh, 2, "two three");
    fresh.run("CREATE notes:3 = { title: 'no body' };").unwrap();
    assert_eq!(held(&afterwards), held(&beforehand));
}

/// The `documents` and `terms` the store has maintained for `by_body`.
fn held(store: &Store) -> (u64, u64) {
    let mut transaction = store.begin().unwrap();
    let table = Catalog::new(&mut transaction)
        .table_id(NamespaceId::new(1), DatabaseId::new(1), "notes")
        .unwrap()
        .expect("the table");
    let index = Catalog::new(&mut transaction)
        .indexes_on(table)
        .unwrap()
        .into_iter()
        .find(|index| index.name == "by_body")
        .expect("the index");
    let statistics = transaction.search_statistics(&index).unwrap();
    (statistics.documents, statistics.terms)
}

/// Recount the collection from the records themselves and compare.
///
/// The recount uses a bare analyzer rather than the declared `simple` one. That
/// is safe for a **length** because every filter maps one token to one token and
/// only an empty result is dropped, and lowercasing empties nothing — so the two
/// agree on how many tokens there are while disagreeing on their spelling. If a
/// filter that splits or drops tokens is ever added, this line is where it
/// breaks, which is the right place for it to.
fn assert_statistics_match_the_records(store: &Store, session: &mut Session<'_>) {
    let analyzer = Analyzer::default();
    let outcomes = session.run("SELECT * FROM notes;").unwrap();
    let mut documents = 0_u64;
    let mut terms = 0_u64;
    for (_, record) in outcomes[0].records().unwrap() {
        let Value::Object(object) = record else {
            continue;
        };
        let Some(Value::String(text)) = object.get("body") else {
            continue;
        };
        let counted = u64::try_from(analyzer.terms(text).len()).unwrap();
        if counted == 0 {
            continue;
        }
        documents = documents.saturating_add(1);
        terms = terms.saturating_add(counted);
    }
    assert_eq!(
        held(store),
        (documents, terms),
        "the maintained statistics and the records disagree"
    );
}

/// A guard against the recount being vacuous.
///
/// Both sides of `assert_statistics_match_the_records` could be zero and it
/// would still pass, so one case states the numbers literally.
#[test]
fn the_statistics_hold_the_numbers_and_not_nothing() {
    let store = store();
    let mut session = searchable(&store);
    write(&mut session, 1, "one two three");
    write(&mut session, 2, "two two");
    assert_eq!(held(&store), (2, 5));
}

#[test]
fn a_term_test_inside_a_projection_now_reads_the_schema_too() {
    // A consequence of this wave rather than its subject, and pinned here so it
    // is a decision rather than a side effect. A projection used to be evaluated
    // without the searched context, so a `MATCHES` in one silently answered
    // `false` for every record while the identical test in a `WHERE` answered
    // correctly. The score needed that context, the analyzer travels in the same
    // place, and separating them to keep one of the two broken would have been a
    // choice nobody would defend out loud.
    let store = store();
    let mut session = searchable(&store);
    write(&mut session, 1, "lock contention");

    let outcomes = session
        .run("SELECT body MATCHES 'lock' AS hit FROM notes;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(field(&records[0].1, "hit"), &Value::Bool(true));
}
