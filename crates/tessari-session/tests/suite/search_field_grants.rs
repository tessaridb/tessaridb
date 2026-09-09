//! What an index-served read does for a caller whose grant hides the field.
//!
//! # The guard, and why it had never been watched
//!
//! `Session::index_serving_score` resolves the caller's visible field set and
//! refuses to serve an index whose field the grant does not contain; the three
//! bounded walks sit downstream of it. Q-391 recorded that **no test paired a
//! search index with a grant**, so the property rested on reading the guard
//! rather than on watching it refuse — which is the one standard this suite
//! otherwise holds itself to.
//!
//! # Why it is load-bearing rather than defensive
//!
//! An index-served read no longer re-tests the whole condition for a plain
//! conjunction of terms, because the re-test was the query's dominant cost. Three
//! things were riding on that re-test and this is the first: a grant that hides
//! the indexed field makes the predicate false, so believing the index instead
//! would make the field **searchable by somebody who cannot read it** — a
//! disclosure whose answer looks exactly like an ordinary result.
//!
//! # The shape of each case
//!
//! A differential, and deliberately not a refusal. Two users read the *same*
//! statement: one holds the field, the other does not. A test that only watched
//! the narrow caller get nothing would pass just as well against a store that
//! answered nobody, so every case carries the wide caller as its control and
//! asserts the index really is in play for them.
//!
//! # What is asserted, and what deliberately is not
//!
//! **The property, not the mechanism.** The narrow caller must get no records;
//! how the store arranges that is its business. Two mechanisms can deliver it —
//! declining to serve the index, or serving it and re-testing the condition
//! against a record the grant has already redacted — and the first draft of this
//! file asserted `EXPLAIN` reported a scan, which is a claim about the second
//! mechanism not being used. That draft failed against a store that was behaving
//! correctly. A test that pins one of two sound mechanisms turns a legitimate
//! optimisation into a failure.
//!
//! # What the falsification established, case by case
//!
//! The visibility clause in `trusts` was removed and the cases re-run. **The
//! search case failed** — the narrow caller got the record — so that case does
//! watch this guard. **The ordered case still passed**, because an ordered
//! index-served read re-tests the condition against the record the grant has
//! already redacted, and the predicate is false whatever `trusts` said. So the
//! ordered case pins the *property* and not this guard, and saying otherwise
//! would be claiming coverage the run does not support.
//!
//! That asymmetry is the reason the search path is the one to watch: it is the
//! one where believing the index is what the store does on purpose.
//!
//! # Why the fixture is two hundred records and not two
//!
//! The planner measures a candidate index against the table before serving it,
//! so on a table of two an index is never worth serving and both callers get a
//! scan — which makes the two arms identical and the case vacuous. The control
//! is what caught that: it asserts the wide caller **is** served by the index,
//! and it failed on a small fixture.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

const PASSWORD: &str = "correct horse battery";

/// How many filler records the table carries.
///
/// Enough that an index narrows the table by a wide margin, because the planner
/// declines to serve one that does not.
const RECORDS: u64 = 200;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two staff records, a search index over `notes`, an ordered index over `title`
/// and two editors — one who will hold the searched field and one who will not.
fn ready(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
             DEFINE COLLECTION staff;\n\
             DEFINE FIELD notes ON staff TYPE string ANALYZER english;",
        )
        .unwrap();
    for n in 0..RECORDS {
        session
            .run(&format!(
                "CREATE staff:{n} = {{ name: 'person {n}', title: 'engineer', \
                 notes: 'the analytical engine and its cards' }};"
            ))
            .unwrap();
    }
    // The one record the two reads are looking for, so both indexes are
    // selective and the planner has a reason to serve them.
    session
        .run(
            "CREATE staff:9999 = { name: 'grace', title: 'admiral', \
             notes: 'a compiler and a manual of its own' };\n\
             DEFINE INDEX by_notes ON staff FIELDS notes SEARCH;\n\
             DEFINE INDEX by_title ON staff FIELDS title;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "DEFINE USER wide ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER narrow ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         USE NAMESPACE prod; USE DATABASE shop;\n\
         GRANT read ON staff FIELDS name, title, notes TO wide;\n\
         GRANT read ON staff FIELDS name TO narrow;",
    )
    .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// One field of the plan a read explains under.
fn plan(session: &mut Session<'_>, script: &str, field: &str) -> String {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

/// The records a read answered with, by identity.
fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

/// Every field name present in the first record a read answered with.
fn fields_of(session: &mut Session<'_>, script: &str) -> Vec<String> {
    let outcomes = session.run(script).unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Some((_, Value::Object(fields))) = records.first() else {
        panic!("expected a record, got {:?}", records.first());
    };
    let mut names: Vec<String> = fields.keys().cloned().collect();
    names.sort();
    names
}

const SEARCH: &str = "SELECT name FROM staff WHERE notes MATCHES 'compiler';";
const ORDERED: &str = "SELECT name FROM staff WHERE title = 'admiral';";

#[test]
fn a_search_index_serves_the_caller_who_holds_the_field() {
    // The control. Without it the case below passes against a store that
    // answers nobody, which is not the property under test.
    let store = store();
    ready(&store);
    let mut wide = signed_in(&store, "wide");
    assert_eq!(
        plan(&mut wide, &format!("EXPLAIN {SEARCH}"), "access"),
        r#"String("index")"#
    );
    assert_eq!(ids(&mut wide, SEARCH), vec![RecordId::from(9999_i64)]);
}

#[test]
fn a_search_index_does_not_serve_a_caller_whose_grant_hides_the_field() {
    let store = store();
    ready(&store);
    let mut narrow = signed_in(&store, "narrow");
    assert!(
        ids(&mut narrow, SEARCH).is_empty(),
        "a caller who cannot read `notes` searched it and got an answer"
    );
}

#[test]
fn an_ordered_index_does_not_serve_a_caller_whose_grant_hides_the_field() {
    // The same rule on the other bounded walk, because the guard is shared and a
    // test of one walk says nothing about the dispatch of the other.
    let store = store();
    ready(&store);

    let mut wide = signed_in(&store, "wide");
    assert_eq!(
        plan(&mut wide, &format!("EXPLAIN {ORDERED}"), "access"),
        r#"String("index")"#
    );
    assert_eq!(ids(&mut wide, ORDERED), vec![RecordId::from(9999_i64)]);

    let mut narrow = signed_in(&store, "narrow");
    assert!(
        ids(&mut narrow, ORDERED).is_empty(),
        "a caller who cannot read `title` filtered on it and got an answer"
    );
}

#[test]
fn the_hidden_field_is_absent_from_what_the_caller_gets_back() {
    // Which path was taken and what the caller sees are two claims, and the
    // second is the one a disclosure is measured in.
    let store = store();
    ready(&store);
    assert_eq!(
        fields_of(&mut signed_in(&store, "wide"), "SELECT * FROM staff;"),
        vec!["name".to_owned(), "notes".to_owned(), "title".to_owned()]
    );
    assert_eq!(
        fields_of(&mut signed_in(&store, "narrow"), "SELECT * FROM staff;"),
        vec!["name".to_owned()],
        "a field outside the grant reached the caller"
    );
}
