//! Reading the store as it stood at an earlier version.
//!
//! Records are versioned by a suffix on their own key, so a read of the past is
//! the read this store already performs with a different sequence. Almost none
//! of this file is about that. It is about the two things that had to be true
//! before the clause could be offered at all.
//!
//! **An index describes the present.** An index entry carries no version and is
//! derived at commit, so consulting one for a historical read gives today's
//! candidates against yesterday's records — a record that has been updated
//! vanishes from the answer, and one that has been updated *into* the condition
//! appears in it while not satisfying it. Neither raises anything, which is why
//! the tests below assert the row sets of index-served shapes rather than the
//! access path: the wrong answer is the failure, and the plan is only how it
//! happens.
//!
//! **A point that cannot be answered exactly is refused.** Ahead of the store
//! there is no state; below the reclaim floor the versions that stood there have
//! been removed. Both would otherwise return something plausible.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE library; USE DATABASE library;\n\
             DEFINE TABLE docs;\n\
             DEFINE INDEX by_status ON docs FIELDS status;",
        )
        .unwrap();
    session
}

/// The sequence the store has committed up to, which is what `VERSION` names.
///
/// Read rather than counted. Counting statements would encode how many
/// sequences each one spends, which is a fact about the write path and not
/// about this feature.
fn now(store: &Store) -> u64 {
    store.committed_tail().unwrap().get()
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

fn field_of(session: &mut Session<'_>, script: &str, name: &str) -> Value {
    let outcomes = session.run(script).unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let Value::Object(object) = &records[0].1 else {
        panic!("not an object: {:?}", records[0].1);
    };
    object
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .clone()
}

#[test]
fn a_read_at_an_earlier_version_answers_with_what_stood_there() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE docs:1 = { status: 'draft', title: 'on sorting' };")
        .unwrap();
    let before = now(&store);
    session
        .run("UPDATE docs:1 SET status = 'published';")
        .unwrap();

    assert_eq!(
        field_of(&mut session, "SELECT * FROM docs:1;", "status"),
        Value::from("published"),
        "the present is unchanged by the clause existing"
    );
    assert_eq!(
        field_of(
            &mut session,
            &format!("SELECT * FROM docs:1 VERSION {before};"),
            "status"
        ),
        Value::from("draft"),
    );
}

#[test]
fn a_record_written_after_the_version_is_not_in_the_answer() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let before = now(&store);
    session.run("CREATE docs:2 = { status: 'draft' };").unwrap();

    assert_eq!(ids(&mut session, "SELECT * FROM docs;").len(), 2);
    assert_eq!(
        ids(
            &mut session,
            &format!("SELECT * FROM docs VERSION {before};")
        ),
        vec![RecordId::Int(1)],
    );
}

/// The false **negative**, and the reason the guard exists.
///
/// `docs:1` was `draft` at `before` and is `published` now. The index entry
/// under `draft` was removed when the record was updated, so a filter served
/// from `by_status` finds no candidate and answers with nothing — a read of the
/// past reporting that a record which was plainly there was not.
#[test]
fn a_record_that_has_since_changed_is_still_found_at_the_version_it_matched() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let before = now(&store);
    session
        .run("UPDATE docs:1 SET status = 'published';")
        .unwrap();

    assert!(
        ids(&mut session, "SELECT * FROM docs WHERE status = 'draft';").is_empty(),
        "nothing is draft now — the fixture is only interesting if this holds"
    );
    assert_eq!(
        ids(
            &mut session,
            &format!("SELECT * FROM docs WHERE status = 'draft' VERSION {before};")
        ),
        vec![RecordId::Int(1)],
    );
}

/// The false **positive**, which is the worse half.
///
/// `docs:1` is `published` now and was not then. Its present-day index entry
/// under `published` names it, and resolving that name at the old snapshot
/// returns the `draft` record — a row in the answer that does not satisfy the
/// condition it was selected by.
#[test]
fn a_record_that_has_since_matched_is_not_in_an_earlier_answer() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let before = now(&store);
    session
        .run("UPDATE docs:1 SET status = 'published';")
        .unwrap();

    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM docs WHERE status = 'published';"
        ),
        vec![RecordId::Int(1)],
        "it matches now — the fixture is only interesting if this holds"
    );
    assert!(
        ids(
            &mut session,
            &format!("SELECT * FROM docs WHERE status = 'published' VERSION {before};")
        )
        .is_empty(),
    );
}

/// The same failure through the ordering path rather than the filter.
///
/// An ordered index holds its entries in the present order, so a historical read
/// served from one is sorted by values the records no longer hold.
#[test]
fn an_ordering_at_an_earlier_version_sorts_by_what_the_records_held_then() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'a' };").unwrap();
    session.run("CREATE docs:2 = { status: 'b' };").unwrap();
    let before = now(&store);
    // Swapped, so present order and historical order are exact reverses and a
    // read served from the index cannot accidentally agree.
    session.run("UPDATE docs:1 SET status = 'z';").unwrap();

    let outcomes = session
        .run(&format!(
            "SELECT * FROM docs ORDER BY status VERSION {before};"
        ))
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(
        records.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![RecordId::Int(1), RecordId::Int(2)],
        "ordered by the values held at {before}, not by today's"
    );
}

#[test]
fn a_version_ahead_of_the_store_is_refused() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let ahead = now(&store) + 1;

    let refusal = session
        .run(&format!("SELECT * FROM docs VERSION {ahead};"))
        .unwrap_err()
        .to_string();
    assert!(refusal.contains(&ahead.to_string()), "{refusal}");
}

#[test]
fn a_version_below_the_reclaim_floor_is_refused() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let early = now(&store);
    session
        .run("UPDATE docs:1 SET status = 'published';")
        .unwrap();
    session
        .run("UPDATE docs:1 SET status = 'retired';")
        .unwrap();

    // Readable until the versions behind it are removed. Asserted first, so a
    // refusal below cannot be the refusal this store gives to everything.
    assert_eq!(
        field_of(
            &mut session,
            &format!("SELECT * FROM docs:1 VERSION {early};"),
            "status"
        ),
        Value::from("draft"),
    );

    let (namespace, database, table) = docs_ids(&store);
    let reclaimed = store.reclaim_table(namespace, database, table).unwrap();
    assert!(reclaimed.versions > 0, "the pass must remove something");
    assert!(
        store.reclaim_floor().unwrap().get() > early,
        "a pass that removed versions raises the floor above what it removed"
    );

    let refusal = session
        .run(&format!("SELECT * FROM docs:1 VERSION {early};"))
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("reclaim"), "{refusal}");
}

#[test]
fn a_pass_that_removes_nothing_leaves_the_floor_where_it_was() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let (namespace, database, table) = docs_ids(&store);

    // One version per record: nothing is strictly older than what a reader at
    // the floor sees, so there is nothing to remove and no history is lost.
    let reclaimed = store.reclaim_table(namespace, database, table).unwrap();
    assert_eq!(reclaimed.versions, 0);
    assert_eq!(
        store.reclaim_floor().unwrap().get(),
        0,
        "a floor raised by a pass that removed nothing would refuse reads that \
         are still exactly answerable"
    );
}

#[test]
fn a_version_inside_a_transaction_is_refused() {
    let store = store();
    let mut session = ready(&store);
    session.run("CREATE docs:1 = { status: 'draft' };").unwrap();
    let before = now(&store);

    let refusal = session
        .run(&format!(
            "BEGIN; SELECT * FROM docs VERSION {before}; COMMIT;"
        ))
        .unwrap_err();
    assert!(
        matches!(refusal, Error::VersionInsideTransaction { .. }),
        "{refusal:?}"
    );
}

#[test]
fn a_traversal_cannot_be_read_at_an_earlier_version() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE TABLE users;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             RELATE users:1->follows->users:2;",
        )
        .unwrap();
    let before = now(&store);
    // A write after the mark, or `before` would be the committed tail and the
    // read would not be historical at all — the refusal below would then be
    // asserting nothing.
    session
        .run("CREATE users:3 = { handle: 'katherine' };")
        .unwrap();

    // It answers in the present, so the refusal below is about the clause and
    // not about the fixture.
    assert_eq!(
        ids(&mut session, "SELECT * FROM users:1->follows->users;"),
        vec![RecordId::Int(2)],
    );

    let refusal = session
        .run(&format!(
            "SELECT * FROM users:1->follows->users VERSION {before};"
        ))
        .unwrap_err();
    assert!(
        matches!(refusal, Error::NoHistoricalTraversal { .. }),
        "{refusal:?}"
    );
}

/// `version` is contextual, like every other clause word in this grammar.
#[test]
fn version_is_still_a_field_name() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE docs:1 = { status: 'draft', version: 3 };")
        .unwrap();

    assert_eq!(
        field_of(&mut session, "SELECT version FROM docs;", "version"),
        Value::from(3),
    );
    // The word followed by something that is not a sequence is a name, and the
    // read that names it is an ordinary present-day read.
    assert_eq!(
        ids(&mut session, "SELECT * FROM docs ORDER BY version;"),
        vec![RecordId::Int(1)],
    );
}

/// The ids `docs` was given, read from the catalog rather than assumed.
fn docs_ids(
    store: &Store,
) -> (
    tessari_types::NamespaceId,
    tessari_types::DatabaseId,
    tessari_types::TableId,
) {
    let mut transaction = store.begin().unwrap();
    let catalog = tessari_storage::Catalog::new(&mut transaction);
    let namespace = catalog
        .namespaces()
        .unwrap()
        .into_iter()
        .find(|found| found.name == "prod")
        .unwrap();
    let database = catalog
        .databases_in(namespace.id)
        .unwrap()
        .into_iter()
        .find(|found| found.name == "library")
        .unwrap();
    let table = catalog
        .tables_in(namespace.id, database.id)
        .unwrap()
        .into_iter()
        .find(|found| found.name == "docs")
        .unwrap();
    (namespace.id, database.id, table.id)
}
