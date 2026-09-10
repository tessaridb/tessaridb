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
             DEFINE TABLE notes SCHEMALESS;\n\
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
             DEFINE TABLE notes SCHEMALESS;\n\
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
             DEFINE TABLE notes SCHEMALESS;\n\
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

/// Two analyzed and indexed fields, so a projection can put one field's text
/// under the other field's name.
///
/// `searchable` gives one searchable field, and one field cannot shadow itself.
fn two_searchable_fields(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             DEFINE FIELD decoy ON notes TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n\
             DEFINE INDEX by_decoy ON notes FIELDS decoy SEARCH;",
        )
        .unwrap();
    // Record 1 holds the word in `body` and record 2 holds it in `decoy`, so the
    // two fields order the same two records the opposite way round. That
    // opposition is what makes the measurement below able to fail.
    session
        .run(
            "CREATE notes:1 = { body: 'quorum quorum quorum', decoy: 'quiet', title: 'first' };\n\
             CREATE notes:2 = { body: 'quiet', decoy: 'quorum quorum quorum', title: 'second' };",
        )
        .unwrap();
    session
}

#[test]
fn the_two_fields_order_the_records_opposite_ways() {
    // The control for the two tests below, and the whole reason they are not
    // vacuous. Each of them asserts that a read answers in `body`'s order while a
    // projection offers `decoy`'s text; that assertion says nothing unless the
    // two orders are actually different, which is asserted here rather than
    // assumed from how the fixture was written.
    let store = store();
    let mut session = two_searchable_fields(&store);

    assert_eq!(
        ordered(
            &mut session,
            "SELECT * FROM notes ORDER BY search::score(body, 'quorum') DESC;",
        ),
        vec![RecordId::Int(1), RecordId::Int(2)],
    );
    assert_eq!(
        ordered(
            &mut session,
            "SELECT * FROM notes ORDER BY search::score(decoy, 'quorum') DESC;",
        ),
        vec![RecordId::Int(2), RecordId::Int(1)],
    );
}

#[test]
fn a_projection_shadowing_the_searched_field_does_not_change_the_order() {
    // Q-384. `plan::statement::ordered` refuses a projection because "the sort
    // runs *after* it and may name what the projection produced rather than what
    // the index holds", and `scored` copied that refusal — which is why the
    // canonical ranked read, the one that projects a title and a score, gets no
    // bound. The refusal's premise was never measured, so it is measured here,
    // in the shape that discriminates: a projection that puts `decoy`'s text
    // under the name `body`, which the sort key names.
    //
    // The answer is that a score does not read the projected value. It does not
    // read the record's field at all on an index whose postings carry their
    // payload: the number comes from the postings of the query's own terms and
    // the record's identity, so there is nothing in it for a projection to
    // change. Where a key *does* read the record — a path key, a `geo::distance`
    // — the ordering stage overlays the source record beneath the projection for
    // exactly this reason (Q-143).
    let store = store();
    let mut session = two_searchable_fields(&store);

    let outcomes = session
        .run("SELECT decoy AS body FROM notes ORDER BY search::score(body, 'quorum') DESC;")
        .unwrap();
    let records = outcomes[0].records().unwrap();

    // The shadow is real: what the answer carries under `body` is `decoy`'s text
    // and not the field the score was measured over. Without this the test could
    // pass on a projection that quietly kept the original field.
    assert_eq!(
        field(&records[0].1, "body"),
        &Value::String("quiet".to_owned()),
    );
    assert_eq!(
        records.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![RecordId::Int(1), RecordId::Int(2)],
        "the order followed the projected text rather than the indexed field",
    );

    // The same projection, ordered by the field whose text it offers, and the
    // answer is the other way round. So the projection does not pin the order:
    // this read is able to produce `[2, 1]`, and the assertion above is a
    // statement about which field the key measured rather than about a fixture
    // that could only ever answer one way.
    assert_eq!(
        ordered(
            &mut session,
            "SELECT decoy AS body FROM notes ORDER BY search::score(decoy, 'quorum') DESC;",
        ),
        vec![RecordId::Int(2), RecordId::Int(1)],
    );
}

#[test]
fn a_projection_dropping_the_searched_field_does_not_change_the_order() {
    // The other half of the fixture the question named: a projection that drops
    // the field the sort key reads, rather than replacing it. Same answer, and
    // it is worth its own test because it fails through a different mechanism —
    // a key evaluated against the projection alone would find nothing here, and
    // an absence that scores as an absence makes every record tie, which is the
    // silent reordering Q-143 was raised for.
    let store = store();
    let mut session = two_searchable_fields(&store);

    let outcomes = session
        .run("SELECT title FROM notes ORDER BY search::score(body, 'quorum') DESC;")
        .unwrap();
    let records = outcomes[0].records().unwrap();

    let Value::Object(first) = &records[0].1 else {
        panic!("not an object: {:?}", records[0].1);
    };
    assert!(!first.contains_key("body"), "the projection kept `body`");
    assert_eq!(
        records.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![RecordId::Int(1), RecordId::Int(2)],
    );
}

/// A record with no text in the searched field scores zero, not `NONE`.
///
/// The twin of `a_record_with_no_text_in_the_field_answers_no_marks_rather_than_none`
/// in `highlighting.rs`, and it exists for the same reason: on a mixed
/// collection an absence that answered `NONE` would give the projection two
/// shapes, and — worse — would reorder the read. The comparison a `DESC` order
/// makes between an absence and a zero is not the comparison it makes between
/// two zeroes, so a record that merely lacks the field could sort above records
/// that were searched and found wanting.
///
/// **What this pins, and what it honestly does not.** `Function::SearchScore` is
/// listed by `Function::answers_for_absence`, and that membership is
/// unobservable (Q-388): `evaluate` intercepts the function in its
/// `ExprKind::Call` arm and returns before `call`, and `call` is the only reader
/// of the list. So this test cannot watch the list. It watches the **answer the
/// list exists to protect** — if the function were ever routed back through
/// `call` and the entry had been dropped in the meantime, `call` would
/// short-circuit to `Value::None` and this assertion would fail. That is the
/// consequence, not the mechanism, and claiming otherwise would be claiming
/// coverage the run does not support.
#[test]
fn a_record_with_no_text_in_the_field_scores_zero_rather_than_none() {
    let store = store();
    let mut session = searchable(&store);
    write(&mut session, 1, "a note mentioning locks once");
    // No `body` at all, which the fixture's other records cannot express.
    session
        .run("CREATE notes:2 = { title: 'this record carries no body' };")
        .unwrap();

    let outcomes = session
        .run(
            "SELECT search::score(body, 'locks') AS relevance \
             FROM notes ORDER BY relevance DESC;",
        )
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 2);

    assert_eq!(
        records[0].0,
        RecordId::Int(1),
        "the record that matched did not come first: {records:?}"
    );
    assert!(number(field(&records[0].1, "relevance")) > 0.0);
    assert_eq!(
        number(field(&records[1].1, "relevance")),
        0.0,
        "an absent field scored something other than zero",
    );
}
