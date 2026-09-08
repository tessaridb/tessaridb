//! Changing one field of a record without rewriting the whole one.
//!
//! Three rules carry the weight, and each is here because the alternative is a
//! surprise rather than because it was hard:
//!
//! - **every right-hand side sees the record as it was**, so `SET a = b, b = a`
//!   swaps rather than assigning `b` to both;
//! - **assigning `none` removes the field**, because `none` means the field is
//!   not there;
//! - **a missing intermediate is refused, never created**, because writing
//!   structure nobody asked for is the same call as zero-filling a hole.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users SCHEMALESS;\n\
             CREATE users:1 = { name: 'ada', city: 'Paris', visits: 3, \
                                address: { city: 'Paris', zip: '75001' } };\n\
             CREATE users:2 = { name: 'grace', city: 'Lyon', visits: 1 };",
        )
        .unwrap();
    session
}

/// The whole record, as text — because what a field update gets wrong is the
/// fields nobody was looking at.
fn record(session: &mut Session<'_>, script: &str) -> String {
    let outcomes = session.run(script).unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    format!("{:?}", records[0].1)
}

fn field(session: &mut Session<'_>, script: &str, name: &str) -> String {
    let outcomes = session.run(script).unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object");
    };
    format!("{:?}", fields.get(name).unwrap_or(&Value::None))
}

#[test]
fn one_field_changes_and_the_rest_do_not() {
    let store = store();
    let mut session = ready(&store);
    let before = record(&mut session, "SELECT * FROM users:1;");
    session
        .run("UPDATE users:1 SET name = 'ada lovelace';")
        .unwrap();
    let after = record(&mut session, "SELECT * FROM users:1;");
    assert_ne!(before, after);
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "name"),
        r#"String("ada lovelace")"#
    );
    // Everything else, verbatim.
    for (name, held) in [
        ("city", r#"String("Paris")"#),
        ("visits", "Number(Integer(3))"),
    ] {
        assert_eq!(field(&mut session, "SELECT * FROM users:1;", name), held);
    }
    assert!(
        after.contains("75001"),
        "the nested value was lost: {after}"
    );
}

#[test]
fn several_assignments_all_apply_and_may_read_the_record() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("UPDATE users:1 SET visits = visits + 1, city = 'Lyon';")
        .unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "visits"),
        "Number(Integer(4))"
    );
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "city"),
        r#"String("Lyon")"#
    );
}

#[test]
fn every_right_hand_side_sees_the_record_as_it_was() {
    // The rule that decides what a statement *means*. Left to right, this would
    // put `city` into both; the statement's meaning would then depend on the
    // order somebody happened to type its clauses in.
    let store = store();
    let mut session = ready(&store);
    session
        .run("UPDATE users:1 SET name = city, city = name;")
        .unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "name"),
        r#"String("Paris")"#
    );
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "city"),
        r#"String("ada")"#
    );
}

#[test]
fn assigning_none_removes_the_field() {
    // `none` means the field is not there, so storing it would say the field is
    // there and holds not-being-there.
    let store = store();
    let mut session = ready(&store);
    session.run("UPDATE users:1 SET city = NONE;").unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "city"),
        "None",
        "the field is still there"
    );
    // …and the *nested* `city` is a different route, so it stayed — which is
    // what made the first version of this assertion pass for the wrong reason.
    let after = record(&mut session, "SELECT * FROM users:1;");
    assert!(after.contains("75001"), "{after}");
    // …and `null` is a value, so it stays. The two are different questions and
    // this is where the difference shows.
    session.run("UPDATE users:2 SET city = NULL;").unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:2;", "city"),
        "Null"
    );
}

#[test]
fn a_nested_route_changes_only_what_it_names() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("UPDATE users:1 SET address.city = 'Lyon';")
        .unwrap();
    let after = record(&mut session, "SELECT * FROM users:1;");
    assert!(after.contains("Lyon"), "{after}");
    assert!(after.contains("75001"), "the sibling was lost: {after}");
    // The top-level `city` is a different route and did not move.
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "city"),
        r#"String("Paris")"#
    );
}

#[test]
fn a_route_the_record_does_not_have_is_refused_rather_than_created() {
    let store = store();
    let mut session = ready(&store);
    for script in [
        "UPDATE users:1 SET meta.source = 'import';",
        "UPDATE users:1 SET address.country.code = 'FR';",
        // A position: assigning into an array by index is its own question.
        "UPDATE users:1 SET address[0] = 'x';",
    ] {
        let refused = session.run(script);
        assert!(refused.is_err(), "{script} was accepted: {refused:?}");
    }
    // …and nothing was written on the way to being refused.
    let after = record(&mut session, "SELECT * FROM users:1;");
    assert!(!after.contains("meta"), "{after}");
    assert!(!after.contains("country"), "{after}");
}

#[test]
fn a_record_that_is_not_there_is_refused() {
    let store = store();
    let mut session = ready(&store);
    let refused = session.run("UPDATE users:99 SET name = 'nobody';");
    assert!(refused.is_err(), "{refused:?}");
}

#[test]
fn the_schema_still_decides_what_may_land() {
    // The result is an ordinary record write, so everything the store already
    // checks on its apply path checks this too — including an assertion, which
    // is the newest of them.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE FIELD visits ON users TYPE int ASSERT $value >= 0;")
        .unwrap();
    let refused = session.run("UPDATE users:1 SET visits = 0 - 5;");
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "visits"),
        "Number(Integer(3))",
        "a refused update changed the record"
    );
    session.run("UPDATE users:1 SET visits = 7;").unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "visits"),
        "Number(Integer(7))"
    );
}

#[test]
fn a_default_applies_to_the_result_the_way_it_does_to_a_whole_record_write() {
    // One rule rather than two, which is what keeps `REQUIRED` + `DEFAULT`
    // meaning "this field always holds a value" — even when a caller sets that
    // field to `none`.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE FIELD tier ON users TYPE string DEFAULT 'bronze';")
        .unwrap();
    session.run("UPDATE users:1 SET tier = 'gold';").unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "tier"),
        r#"String("gold")"#
    );
    session.run("UPDATE users:1 SET tier = NONE;").unwrap();
    assert_eq!(
        field(&mut session, "SELECT * FROM users:1;", "tier"),
        r#"String("bronze")"#,
        "a field with a default did not get one back"
    );
}

#[test]
fn an_index_keeps_up_because_this_is_an_ordinary_write() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_city ON users FIELDS city;")
        .unwrap();
    session.run("UPDATE users:2 SET city = 'Paris';").unwrap();

    let outcomes = session
        .run("SELECT * FROM users WHERE city = 'Paris';")
        .unwrap();
    assert_eq!(outcomes[0].path().unwrap(), AccessPath::Index);
    assert_eq!(outcomes[0].records().unwrap().len(), 2);
    // …and the entry the record left behind is gone.
    let outcomes = session
        .run("SELECT * FROM users WHERE city = 'Lyon';")
        .unwrap();
    assert!(outcomes[0].records().unwrap().is_empty());
}

#[test]
fn the_whole_record_form_still_replaces_the_record() {
    // One statement, two shapes, and the older one keeps its meaning: giving a
    // value replaces, and `SET` changes.
    let store = store();
    let mut session = ready(&store);
    session
        .run("UPDATE users:1 = { name: 'only this' };")
        .unwrap();
    let after = record(&mut session, "SELECT * FROM users:1;");
    assert!(after.contains("only this"), "{after}");
    assert!(
        !after.contains("visits"),
        "the replace did not replace: {after}"
    );
}

// ------------------------------------------------- the reason `SET` exists

/// How many times the concurrency invariant is re-run.
///
/// The same constant `tessari-storage`'s isolation suite uses, for the same
/// reason: a scheduling-dependent assertion that went green once has proved
/// nothing about the schedules it did not happen to take.
const CONCURRENCY_RUNS: u32 = 25;
/// Sessions competing for one field in each run.
const WRITERS: u32 = 8;

/// Eight sessions increment one field, and all eight land.
///
/// # This is the property a client cannot build for itself
///
/// The convenience half of `SET` — not rewriting the fields you did not mean to
/// change — is what it looks like it is for. The correctness half is this:
/// `visits = visits + 1` **reads and writes inside one transaction**, so the
/// read cannot go stale between the two. A client doing the same job reads in
/// one call and writes in another, and every interleaving in between is an
/// update it silently overwrote.
///
/// Asserted **against the count**, not against "it did not error". A lost update
/// raises nothing anywhere — that is the entire difficulty of it — so the only
/// witness is arithmetic: eight increments from zero is eight, and any smaller
/// number names exactly how many were lost.
///
/// A conflict is retried rather than tolerated. A conflict means this session's
/// snapshot was stale, and retrying on a fresh one is the correct response;
/// counting a conflict as a landed write would make the assertion vacuous.
#[test]
fn concurrent_sessions_incrementing_one_field_all_land() {
    use std::thread;

    for run in 0..CONCURRENCY_RUNS {
        let store = store();
        {
            let mut session = ready(&store);
            session.run("UPDATE users:1 SET visits = 0;").unwrap();
        }

        thread::scope(|scope| {
            for _ in 0..WRITERS {
                let store = store.clone();
                scope.spawn(move || {
                    let mut session = Session::new(&store);
                    session
                        .run("USE NAMESPACE prod; USE DATABASE shop;")
                        .unwrap();
                    loop {
                        match session.run("UPDATE users:1 SET visits = visits + 1;") {
                            Ok(_) => break,
                            Err(error) if retryable(&error) => (),
                            Err(other) => panic!("run {run}: unexpected error: {other}"),
                        }
                    }
                });
            }
        });

        let mut session = Session::new(&store);
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        let landed = field(&mut session, "SELECT * FROM users:1;", "visits");
        assert_eq!(
            landed,
            format!("Number(Integer({WRITERS}))"),
            "run {run}: an update was lost"
        );
    }
}

/// Whether a failed write is one this session should simply take again.
///
/// A conflict says the snapshot read was overtaken; contention says the commit
/// could not get through. Both are answered by trying again on a fresh
/// snapshot, and nothing else here is.
fn retryable(error: &tessari_session::Error) -> bool {
    matches!(
        error,
        tessari_session::Error::Store(
            tessari_storage::Error::Conflict { .. }
                | tessari_storage::Error::CommitContention { .. }
        )
    )
}
