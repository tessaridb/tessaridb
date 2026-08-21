//! Following a record reference.
//!
//! A reference is not a value that resembles a key — it *is* the address of the
//! other record. So following one is a point read rather than a search, and
//! these tests are mostly about the cases where there is nothing at the far end,
//! or where the thing being followed is not a reference at all. Those are the
//! cases a caller meets in a real store and the ones a happy-path test misses.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::Session;
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// `posts` referring to `users`, with one post per interesting case.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE TABLE users;\n\
             DEFINE TABLE posts;\n\
             CREATE users:1 = { name: 'ada', city: 'london' };\n\
             CREATE users:2 = { name: 'grace', city: 'york' };\n\
             CREATE posts:1 = { title: 'first',  author: users:1 };\n\
             CREATE posts:2 = { title: 'second', author: users:2 };",
        )
        .unwrap();
    session
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(object) = value else {
        panic!("not an object: {value:?}");
    };
    object
        .get(name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn one(session: &mut Session<'_>, script: &str) -> Value {
    let outcomes = session.run(script).unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1, "expected one record from {script:?}");
    records[0].1.clone()
}

#[test]
fn a_reference_becomes_the_record_it_names() {
    let store = store();
    let mut session = ready(&store);
    let post = one(&mut session, "SELECT * FROM posts:1 FETCH author;");
    assert_eq!(field(field(&post, "author"), "name"), &Value::from("ada"));
}

#[test]
fn without_the_clause_it_stays_a_reference() {
    // The feature has to be asked for: a read that did not write `FETCH` must
    // not silently pay a point read per record.
    let store = store();
    let mut session = ready(&store);
    let post = one(&mut session, "SELECT * FROM posts:1;");
    assert!(
        matches!(field(&post, "author"), Value::Record(_)),
        "{post:?}"
    );
}

#[test]
fn a_projection_reads_into_the_fetched_record() {
    // The statement the clause exists for, and the reason fetching happens
    // before the projection rather than after it.
    let store = store();
    let mut session = ready(&store);
    let post = one(
        &mut session,
        "SELECT author.name AS by FROM posts:1 FETCH author;",
    );
    assert_eq!(field(&post, "by"), &Value::from("ada"));
}

#[test]
fn an_order_by_sorts_on_a_fetched_value() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT * FROM posts FETCH author ORDER BY author.name DESC;")
        .unwrap();
    let order: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    // `grace` before `ada`.
    assert_eq!(order, vec![RecordId::Int(2), RecordId::Int(1)]);
}

#[test]
fn a_reference_to_nothing_stays_a_reference() {
    // A record can be deleted while something still names it. The field keeps
    // the name — the one piece of information the caller has — so an object
    // means the record was there and a reference means it was not.
    let store = store();
    let mut session = ready(&store);
    session.run("DELETE users:2;").unwrap();

    let post = one(&mut session, "SELECT * FROM posts:2 FETCH author;");
    assert_eq!(
        field(&post, "author"),
        &Value::Record(bgv_db_types::RecordRef::new(
            match field(&post, "author") {
                Value::Record(held) => held.table,
                other => panic!("not a reference: {other:?}"),
            },
            RecordId::Int(2)
        ))
    );
}

#[test]
fn an_array_of_references_is_followed_element_by_element() {
    // A list of references is how a to-many relation is stored here, so the
    // clause would be half a feature without this.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE posts:3 = { title: 'third', authors: [users:1, users:2] };")
        .unwrap();

    let post = one(&mut session, "SELECT * FROM posts:3 FETCH authors;");
    let Value::Array(authors) = field(&post, "authors") else {
        panic!("not an array: {post:?}");
    };
    assert_eq!(authors.len(), 2);
    assert_eq!(field(&authors[0], "name"), &Value::from("ada"));
    assert_eq!(field(&authors[1], "name"), &Value::from("grace"));
}

#[test]
fn an_element_that_is_not_a_reference_is_left_alone() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE posts:4 = { mixed: [users:1, 'a string', 7] };")
        .unwrap();

    let post = one(&mut session, "SELECT * FROM posts:4 FETCH mixed;");
    let Value::Array(items) = field(&post, "mixed") else {
        panic!("not an array");
    };
    assert_eq!(field(&items[0], "name"), &Value::from("ada"));
    assert_eq!(items[1], Value::from("a string"));
}

#[test]
fn a_field_that_is_not_a_reference_and_a_route_to_nothing_are_both_untouched() {
    // The missing-field rule, one level down: a route that reaches nothing is
    // not an error anywhere else in this language and is not one here.
    let store = store();
    let mut session = ready(&store);
    let post = one(&mut session, "SELECT * FROM posts:1 FETCH title, nowhere;");
    assert_eq!(field(&post, "title"), &Value::from("first"));
    assert!(matches!(field(&post, "author"), Value::Record(_)));
}

#[test]
fn a_nested_route_is_followed_and_only_one_level_is() {
    // Two claims in one fixture: `meta.editor` reaches through an object, and
    // the record it finds keeps its own references as references — which is what
    // bounds the work and makes a cycle impossible rather than handled.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE users:3 = { name: 'edith', mentor: users:1 };\n\
             CREATE posts:5 = { title: 'fifth', meta: { editor: users:3 } };",
        )
        .unwrap();

    let post = one(&mut session, "SELECT * FROM posts:5 FETCH meta.editor;");
    let editor = field(field(&post, "meta"), "editor");
    assert_eq!(field(editor, "name"), &Value::from("edith"));
    assert!(
        matches!(field(editor, "mentor"), Value::Record(_)),
        "the fetched record's own reference was followed: {editor:?}"
    );
}

#[test]
fn several_routes_are_followed_in_one_read() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE posts:6 = { author: users:1, reviewer: users:2 };")
        .unwrap();

    let post = one(
        &mut session,
        "SELECT * FROM posts:6 FETCH author, reviewer;",
    );
    assert_eq!(field(field(&post, "author"), "name"), &Value::from("ada"));
    assert_eq!(
        field(field(&post, "reviewer"), "name"),
        &Value::from("grace")
    );
}

#[test]
fn fetch_is_still_usable_as_a_field_name() {
    // Contextual and not reserved, the same decision `ORDER`, `GROUP`, `START`
    // and `LIMIT` took. A language that takes a common noun away from its users
    // to buy a clause has made a poor trade.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE posts:7 = { fetch: 'a field called fetch', author: users:1 };")
        .unwrap();

    let post = one(&mut session, "SELECT * FROM posts:7 FETCH author;");
    assert_eq!(field(&post, "fetch"), &Value::from("a field called fetch"));
    assert_eq!(field(field(&post, "author"), "name"), &Value::from("ada"));

    // And as a projected name.
    let named = one(&mut session, "SELECT fetch FROM posts:7;");
    assert_eq!(field(&named, "fetch"), &Value::from("a field called fetch"));
}

#[test]
fn a_filtered_read_fetches_too() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT * FROM posts WHERE title = 'second' FETCH author;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        field(field(&records[0].1, "author"), "name"),
        &Value::from("grace")
    );
}

#[test]
fn one_reference_named_many_times_is_read_once_and_answers_the_same() {
    // The saving is stated in the module documentation, so the property it rests
    // on is asserted here: a read resolves at one snapshot, so two reads of one
    // address must answer the same thing — which is what makes remembering the
    // first a rewrite rather than a cache that can go stale.
    //
    // The count itself is not observable from outside; what is observable is
    // that every one of the many gets the same record, which is the half that
    // could break if the memo were keyed wrongly.
    let store = store();
    let mut session = ready(&store);
    for id in 10..20 {
        session
            .run(&format!("CREATE posts:{id} = {{ author: users:1 }};"))
            .unwrap();
    }

    let outcomes = session
        .run("SELECT * FROM posts WHERE author = users:1 FETCH author;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 11);
    for (_, post) in records {
        assert_eq!(field(field(post, "author"), "name"), &Value::from("ada"));
    }
}

#[test]
fn two_routes_naming_two_records_do_not_get_each_others_answers() {
    // The memo is keyed by table and id, so this is the test that catches a key
    // that dropped one of them.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE posts:20 = { author: users:1, reviewer: users:2 };\n\
             CREATE posts:21 = { author: users:2, reviewer: users:1 };",
        )
        .unwrap();

    let first = one(
        &mut session,
        "SELECT * FROM posts:20 FETCH author, reviewer;",
    );
    assert_eq!(field(field(&first, "author"), "name"), &Value::from("ada"));
    assert_eq!(
        field(field(&first, "reviewer"), "name"),
        &Value::from("grace")
    );

    let outcomes = session
        .run("SELECT * FROM posts WHERE title = NONE FETCH author, reviewer;")
        .unwrap();
    for (id, post) in outcomes[0].records().unwrap() {
        let (author, reviewer) = match id {
            RecordId::Int(20) => ("ada", "grace"),
            RecordId::Int(21) => ("grace", "ada"),
            _ => continue,
        };
        assert_eq!(field(field(post, "author"), "name"), &Value::from(author));
        assert_eq!(
            field(field(post, "reviewer"), "name"),
            &Value::from(reviewer)
        );
    }
}
