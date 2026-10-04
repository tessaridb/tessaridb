//! An index records the tokenizer generation that built its terms, and one that
//! may hold an earlier tokenizer's terms is not answered from (G058 C3, Q-911).
//!
//! The failure this exists for returns success: `0.22.0-beta` made each Chinese
//! or Japanese ideograph a token, and an index written before still held whole
//! sentences, so a read through it answered fewer records than the scan with
//! nothing in an error state. The store cannot tell an index written by
//! `0.22`–`0.25` from one written by `0.21`, so an index that recorded no
//! generation is treated as one that may be stale: a field's index scans
//! instead, a search's member is read and says it can miss records, and one
//! statement rebuilds either.
//!
//! The legacy state is written the way such a store holds it — the definition
//! with no generation — through the catalog, which writes a rebuilt definition
//! exactly as it is given.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Note, Outcome, Session};
use tessari_storage::{Catalog, Store};
use tessari_types::{TOKENIZER_GENERATION, Value};

const USE: &str = "USE NAMESPACE prod; USE DATABASE shop;";
const READ: &str = "SELECT id FROM notes WHERE body MATCHES '東京';";

fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store)
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE ANALYZER plain FILTERS lowercase, ascii;\n\
             DEFINE COLLECTION notes;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER plain;\n\
             CREATE notes:1 = { body: '東京都の地図' };\n\
             CREATE notes:2 = { body: '京都の寺' };\n\
             CREATE notes:3 = { body: 'a map of Tokyo' };\n\
             DEFINE INDEX by_body ON notes FIELDS body SEARCH;\n\
             DEFINE SEARCH kb ON notes FIELDS body ANALYZER plain;",
        )
        .unwrap();
    store
}

/// Write `index` on `notes` back with the generation `built`, as a store written
/// by an earlier build holds it.
fn written_by(store: &Store, index: &str, built: Option<u32>) {
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "notes")
        .unwrap()
        .unwrap();
    let mut definition = catalog
        .indexes_on(table)
        .unwrap()
        .into_iter()
        .find(|held| held.name == index)
        .unwrap();
    definition.tokenizer = built;
    catalog.rebuild_index(&definition);
    transaction.commit().unwrap();
}

/// The ids a read answered with, sorted, its path, and its rebuild notes.
fn read(session: &mut Session<'_>, statement: &str) -> (Vec<String>, AccessPath, Vec<Note>) {
    let outcomes = session.run(statement).unwrap();
    let Some(outcome @ Outcome::Records { records, plan, .. }) = outcomes.last() else {
        panic!("{statement}: {:?}", outcomes.last());
    };
    let mut ids: Vec<String> = records.iter().map(|(id, _)| id.to_string()).collect();
    ids.sort();
    let rebuild = outcome
        .notes()
        .iter()
        .filter(|note| note.kind() == "needs-rebuild")
        .cloned()
        .collect();
    (ids, plan.access, rebuild)
}

/// `(tokenizer, rebuild)` for one entry of a report's list.
fn generation_of(entries: &Value, named: &str, key: &str) -> (Value, Value) {
    let Value::Array(entries) = entries else {
        panic!("not a list: {entries:?}");
    };
    for entry in entries {
        let Value::Object(fields) = entry else {
            continue;
        };
        if fields.get(key) == Some(&Value::from(named)) {
            return (
                fields.get("tokenizer").cloned().unwrap_or(Value::None),
                fields.get("rebuild").cloned().unwrap_or(Value::None),
            );
        }
    }
    panic!("{named} is not in {entries:?}");
}

fn field_index(session: &mut Session<'_>) -> (Value, Value) {
    let outcomes = session.run("INFO FOR TABLE notes;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("{:?}", outcomes.last());
    };
    generation_of(&report["indexes"], "by_body", "name")
}

fn member(session: &mut Session<'_>) -> (Value, Value) {
    let outcomes = session.run("INFO FOR SEARCH kb;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("{:?}", outcomes.last());
    };
    generation_of(&report["members"], "notes", "table")
}

fn current() -> Value {
    Value::from(i64::from(TOKENIZER_GENERATION))
}

#[test]
fn a_new_search_index_records_its_generation_and_is_answered_from() {
    let store = store();
    let mut session = Session::new(&store);
    session.run(USE).unwrap();
    assert_eq!(field_index(&mut session), (current(), Value::Bool(false)));
    assert_eq!(member(&mut session), (current(), Value::Bool(false)));
    let (ids, path, notes) = read(&mut session, READ);
    assert_eq!(ids, ["1"]);
    assert_eq!(path, AccessPath::Index);
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_field_index_of_another_or_no_generation_is_scanned_past_until_rebuilt() {
    for built in [None, Some(1)] {
        let store = store();
        written_by(&store, "by_body", built);
        let mut session = Session::new(&store);
        session.run(USE).unwrap();
        let expected_tokenizer =
            built.map_or(Value::Null, |generation| Value::from(i64::from(generation)));
        assert_eq!(
            field_index(&mut session),
            (expected_tokenizer, Value::Bool(true))
        );

        // The same records the index would give, from the scan, and said so.
        let (ids, path, notes) = read(&mut session, READ);
        assert_eq!(ids, ["1"], "built {built:?}");
        assert_eq!(
            path,
            AccessPath::Scan,
            "built {built:?}: answered from the index"
        );
        assert_eq!(
            notes,
            [Note::NeedsRebuild {
                index: "by_body".to_owned(),
                table: "notes".to_owned(),
                built,
                member: false,
            }]
        );
        assert!(
            notes[0]
                .message()
                .contains("REBUILD INDEX by_body ON notes"),
            "{}",
            notes[0].message()
        );
        // Asserting the index is refused rather than quietly scanned.
        match session.run(&format!(
            "{} USING INDEX by_body;",
            READ.trim_end_matches(';')
        )) {
            Err(Error::IndexNotUsed { expected, .. }) => assert_eq!(expected, "by_body"),
            other => panic!("built {built:?}: USING INDEX was not refused: {other:?}"),
        }

        session.run("REBUILD INDEX by_body ON notes;").unwrap();
        assert_eq!(field_index(&mut session), (current(), Value::Bool(false)));
        let (ids, path, notes) = read(&mut session, READ);
        assert_eq!(ids, ["1"]);
        assert_eq!(
            path,
            AccessPath::Index,
            "built {built:?}: not served after the rebuild"
        );
        assert!(notes.is_empty(), "{notes:?}");
    }
}

#[test]
fn a_search_member_of_no_generation_is_read_and_says_it_can_miss_records() {
    let store = store();
    written_by(&store, "kb", None);
    let mut session = Session::new(&store);
    session.run(USE).unwrap();
    assert_eq!(member(&mut session), (Value::Null, Value::Bool(true)));
    let ask = "SELECT id FROM SEARCH kb MATCHES '東京';";
    let (ids, _, notes) = read(&mut session, ask);
    assert_eq!(ids, ["1"]);
    assert_eq!(
        notes,
        [Note::NeedsRebuild {
            index: "kb".to_owned(),
            table: "notes".to_owned(),
            built: None,
            member: true,
        }]
    );

    session
        .run("BEGIN; DROP SEARCH kb; DEFINE SEARCH kb ON notes FIELDS body ANALYZER plain; COMMIT;")
        .unwrap();
    assert_eq!(member(&mut session), (current(), Value::Bool(false)));
    let (ids, _, notes) = read(&mut session, ask);
    assert_eq!(ids, ["1"]);
    assert!(notes.is_empty(), "{notes:?}");
}

/// A reader whose grant hides the field is not told an index on it needs
/// rebuilding: as far as it can tell, the field has no index at all.
#[test]
fn a_field_the_grant_hides_names_no_index_in_the_note() {
    const PASSWORD: &str = "correct horse battery";
    let store = store();
    written_by(&store, "by_body", None);
    let mut root = Session::new(&store);
    root.run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE FIELD title ON notes TYPE string;\n\
         DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         GRANT read ON notes FIELDS title TO ada;",
    )
    .unwrap();
    let (_, _, owner) = read(&mut root, READ);
    assert_eq!(owner.len(), 1, "the owner is told: {owner:?}");

    let mut ada = Session::new(&store);
    ada.sign_in("ada", PASSWORD).unwrap();
    ada.run(USE).unwrap();
    let (_, _, hidden) = read(&mut ada, READ);
    assert!(
        hidden.is_empty(),
        "a hidden field's index was named: {hidden:?}"
    );
}
