//! What a caller sees, and what a caller must not have to see.
//!
//! These tests are written from outside: nothing here imports a crate below the
//! facade, no `Arc` appears, and no trait object is named. That is the point of
//! the wave — if any of it were necessary, the store's insides would still be
//! its interface.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessaridb::{
    AccessPath, Change, ChangeKind, Db, Exactness, Reach, RecordId, Sequence, Value, Watch,
};

/// The script every test starts from, so each one says only what it is about.
/// Where `READY`'s records land.
///
/// The first namespace and the first database this fixture declares, which a log
/// is now kept per (S6.2) — so a feed asked about the store's own log would find
/// the definitions and none of the records.
const FIXTURE_HOME: Reach = Reach::Database(
    tessaridb::NamespaceId::new(1),
    tessaridb::DatabaseId::new(1),
);

const READY: &str = "DEFINE NAMESPACE prod;\
                     USE NAMESPACE prod;\
                     DEFINE DATABASE orders;\
                     USE DATABASE orders;\
                     DEFINE COLLECTION users;";

fn name_of(record: &Value) -> Option<&str> {
    match record {
        Value::Object(fields) => match fields.get("name") {
            Some(Value::String(name)) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn opening_a_database_takes_one_line_and_names_nothing_internal() {
    let db = Db::in_memory().unwrap();
    let mut session = db.session();
    session.run(READY).unwrap();
    session
        .run("CREATE users:1 = { name: 'ada', city: 'Paris' };")
        .unwrap();

    let found = session.run("SELECT name FROM users:1;").unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Record));
    // Beside the path, and for the same reason it is here: a caller of the front
    // door can ask how the records were reached and whether they are provably
    // the ones the question named, without unpacking the plan. The second is not
    // an extra — a caller that cannot reach it is the caller G022's S7 is about.
    assert_eq!(found[0].exactness(), Some(Exactness::Exact));
    let records = found[0].records().unwrap();
    assert_eq!(records[0].0, RecordId::Int(1));
    assert_eq!(name_of(&records[0].1), Some("ada"));
}

#[test]
fn the_two_ways_to_open_run_the_same_script_to_the_same_result() {
    // The claim the whole storage layer is built to support, stated at the
    // surface a caller actually touches rather than only at the substrate.
    let script = "CREATE users:1 = { name: 'ada' };\
                  CREATE users:2 = { name: 'grace' };\
                  DEFINE INDEX by_name ON users FIELDS name;\
                  SELECT * FROM users WHERE name = 'grace';";

    let memory = Db::in_memory().unwrap();
    let mut session = memory.session();
    session.run(READY).unwrap();
    let from_memory = session.run(script).unwrap();

    let root = tempfile::tempdir().unwrap();
    let persistent = Db::open(root.path().join("store")).unwrap();
    let mut session = persistent.session();
    session.run(READY).unwrap();
    let from_disk = session.run(script).unwrap();

    assert_eq!(from_memory, from_disk);
    assert_eq!(from_disk.last().unwrap().path(), Some(AccessPath::Index));
}

#[test]
fn a_database_opened_at_a_path_remembers_what_was_written_to_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    {
        let db = Db::open(&path).unwrap();
        let mut session = db.session();
        session.run(READY).unwrap();
        session.run("CREATE users:1 = { name: 'ada' };").unwrap();
    }

    let reopened = Db::open(&path).unwrap();
    let mut session = reopened.session();
    session.run("USE NAMESPACE prod DATABASE orders;").unwrap();
    let found = session.run("SELECT * FROM users;").unwrap();
    assert_eq!(found[0].records().unwrap().len(), 1);
}

#[test]
fn two_sessions_on_one_database_are_two_conversations() {
    let db = Db::in_memory().unwrap();
    let mut first = db.session();
    first.run(READY).unwrap();
    first.run("CREATE users:1 = { name: 'ada' };").unwrap();

    // The second has selected nothing, so it cannot see the table by name until
    // it says so itself — which is what makes them independent.
    let mut second = db.session();
    assert!(second.run("SELECT * FROM users;").is_err());
    second.run("USE NAMESPACE prod DATABASE orders;").unwrap();
    assert_eq!(
        second.run("SELECT * FROM users;").unwrap()[0]
            .records()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn the_change_feed_reaches_the_surface() {
    let db = Db::in_memory().unwrap();
    let mut session = db.session();
    session.run(READY).unwrap();
    session.run("CREATE users:1 = { name: 'ada' };").unwrap();
    session.run("DELETE users:1;").unwrap();

    let answer = db
        .changes_since(
            db.store().own_log(FIXTURE_HOME).unwrap(),
            Sequence::ZERO,
            1024,
        )
        .unwrap();
    let kinds: Vec<&ChangeKind> = answer.changes.iter().map(|c| &c.kind).collect();
    assert_eq!(kinds.len(), 2, "{kinds:?}");
    assert!(matches!(kinds[0], ChangeKind::Written(_)));
    assert_eq!(kinds[1], &ChangeKind::Removed);
}

#[test]
fn a_subscription_is_a_value_the_caller_keeps() {
    let db = Db::in_memory().unwrap();
    let mut session = db.session();
    session.run(READY).unwrap();

    let mut watching = Db::subscribe(
        db.store().own_log(FIXTURE_HOME).unwrap(),
        Sequence::ZERO,
        Watch::default(),
    );
    session.run("CREATE users:1 = { name: 'ada' };").unwrap();
    let first: Vec<Change> = db.poll(&mut watching, 1024).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
        name_of(match &first[0].kind {
            ChangeKind::Written(value) => value,
            ChangeKind::Removed => panic!("a write was reported as a removal"),
        }),
        Some("ada")
    );

    // It does not repeat, and a skip past the tail is counted rather than
    // silent.
    session.run("CREATE users:2 = { name: 'grace' };").unwrap();
    let tail = db
        .committed_tail(db.store().own_log(FIXTURE_HOME).unwrap())
        .unwrap();
    let skipped = db
        .skip(&mut watching, Sequence::new(tail.get() + 1))
        .unwrap();
    assert_eq!(skipped, 1);
    assert!(db.poll(&mut watching, 1024).unwrap().is_empty());
    assert_eq!(watching.delivered(), 1);
    assert_eq!(watching.dropped(), 1);
}
