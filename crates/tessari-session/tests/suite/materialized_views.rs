//! Materialized views (ADR-0109): kept from one table's changes, always equal
//! to their read at the version they state.
//!
//! The property every test here comes back to is the differential one: the
//! stored rows, read at the view's stated version, are what the view's read
//! answers `VERSION` that version. Each test also pins at least one side to
//! content it wrote, because two derived answers agreeing is agreement and not
//! correctness.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session, maintain_views};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

pub(super) fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

pub(super) fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 0..40_i64 {
        script.push_str(&format!(
            "CREATE orders:{n} = {{ band: {}, total: {}, n: {n} }};\n",
            n % 4,
            n.wrapping_mul(7) % 50
        ));
    }
    session.run(&script).unwrap();
    session
}

pub(super) fn records(session: &mut Session<'_>, script: &str) -> Vec<(RecordId, Value)> {
    let outcomes = session.run(script).unwrap();
    outcomes.last().unwrap().records().unwrap().to_vec()
}

fn refusal(session: &mut Session<'_>, script: &str) -> Error {
    match session.run(script) {
        Err(refused) => refused,
        Ok(outcomes) => panic!("`{script}` answered {outcomes:?}"),
    }
}

/// `INFO FOR TABLE <view>`'s `materialized` object.
fn kept(session: &mut Session<'_>, view: &str) -> std::collections::BTreeMap<String, Value> {
    let outcomes = session.run(&format!("INFO FOR TABLE {view};")).unwrap();
    let Some(Outcome::Value(Value::Object(info))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    let Some(Value::Object(kept)) = info.get("materialized") else {
        panic!("not reported as materialized: {info:?}");
    };
    kept.clone()
}

fn number(fields: &std::collections::BTreeMap<String, Value>, name: &str) -> i64 {
    match fields.get(name) {
        Some(Value::Number(Number::Integer(held))) => *held,
        other => panic!("{name} was {other:?}"),
    }
}

/// The view's rows against its read at the version it states. A per-record
/// view keeps the source identities, so they are compared too; any other view
/// keeps positions, so its values are compared in order.
pub(super) fn equals_its_read(session: &mut Session<'_>, view: &str, read: &str, keyed: bool) {
    let version = number(&kept(session, view), "version");
    let stored = records(session, &format!("SELECT * FROM {view};"));
    let expanded = records(session, &format!("{read} VERSION {version};"));
    if keyed {
        assert_eq!(stored, expanded, "`{view}` at version {version}");
    } else {
        let values =
            |rows: &[(RecordId, Value)]| rows.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>();
        assert_eq!(
            values(&stored),
            values(&expanded),
            "`{view}` at version {version}"
        );
    }
}

const BIG: &str = "SELECT n, total FROM orders WHERE total > 30";

#[test]
fn a_kept_view_holds_its_read_as_soon_as_it_is_declared() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW big MATERIALIZED AS {BIG};"))
        .unwrap();
    let stored = records(&mut session, "SELECT * FROM big;");
    // Pinned to content: 40 orders, total = n*7 % 50, more than 30.
    let expected: Vec<RecordId> = (0..40_i64)
        .filter(|n| (n * 7) % 50 > 30)
        .map(RecordId::Int)
        .collect();
    assert_eq!(
        stored.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        expected
    );
    equals_its_read(&mut session, "big", BIG, true);
}

#[test]
fn a_per_record_view_follows_writes_updates_and_deletes() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW big MATERIALIZED AS {BIG};"))
        .unwrap();
    session
        .run(
            "UPDATE orders:1 SET total = 49;\n\
             UPDATE orders:5 SET total = 1;\n\
             DELETE orders:12;\n\
             CREATE orders:100 = { band: 0, total: 44, n: 100 };",
        )
        .unwrap();
    let before = number(&kept(&mut session, "big"), "version");
    // Read from what is stored, not re-run: until it is maintained the view
    // still answers as of its version, without the new order.
    let stale: Vec<RecordId> = records(&mut session, "SELECT * FROM big;")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(!stale.contains(&RecordId::Int(100)) && stale.contains(&RecordId::Int(5)));
    equals_its_read(&mut session, "big", BIG, true);
    let done = maintain_views(&store).unwrap();
    assert_eq!(done.views, 1);
    assert!(number(&kept(&mut session, "big"), "version") > before);
    let ids: Vec<RecordId> = records(&mut session, "SELECT * FROM big;")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(ids.contains(&RecordId::Int(1)) && ids.contains(&RecordId::Int(100)));
    assert!(!ids.contains(&RecordId::Int(5)) && !ids.contains(&RecordId::Int(12)));
    equals_its_read(&mut session, "big", BIG, true);
}

#[test]
fn a_grouped_view_keeps_min_and_max_through_a_delete() {
    let store = store();
    let mut session = ready(&store);
    let read = "SELECT band, count(*) AS orders, max(total) AS top, min(total) AS least \
                FROM orders GROUP BY band";
    session
        .run(&format!("DEFINE VIEW bands MATERIALIZED AS {read};"))
        .unwrap();
    // Remove every band-1 order holding that band's largest total — the case a
    // delta cannot undo without the next largest.
    let rows = records(
        &mut session,
        "SELECT * FROM orders WHERE band = 1 ORDER BY total DESC LIMIT 1;",
    );
    let (top, _) = rows[0].clone();
    session
        .run(&format!(
            "DELETE orders:{};",
            match top {
                RecordId::Int(n) => n,
                other => panic!("{other:?}"),
            }
        ))
        .unwrap();
    maintain_views(&store).unwrap();
    equals_its_read(&mut session, "bands", read, false);
    // A whole group goes: the view loses a row rather than keeping a stale one.
    session
        .run("DELETE FROM orders WHERE band = 3 LIMIT ALL;")
        .unwrap();
    maintain_views(&store).unwrap();
    assert_eq!(records(&mut session, "SELECT * FROM bands;").len(), 3);
    equals_its_read(&mut session, "bands", read, false);
}

#[test]
fn an_ordered_bounded_view_keeps_its_order() {
    let store = store();
    let mut session = ready(&store);
    let read = "SELECT n, total FROM orders ORDER BY total DESC LIMIT 3";
    session
        .run(&format!("DEFINE VIEW top MATERIALIZED AS {read};"))
        .unwrap();
    session.run("UPDATE orders:2 SET total = 99;").unwrap();
    maintain_views(&store).unwrap();
    let first = records(&mut session, "SELECT * FROM top LIMIT 1;");
    let Value::Object(row) = &first[0].1 else {
        panic!()
    };
    assert_eq!(row.get("total"), Some(&Value::from(99_i64)));
    equals_its_read(&mut session, "top", read, false);
}

#[test]
fn a_read_a_kept_view_cannot_follow_is_refused_where_it_is_declared() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE COLLECTION people;").unwrap();
    for (script, what) in [
        (
            "DEFINE VIEW v1 MATERIALIZED AS SELECT * FROM orders WHERE n < time::now();",
            "depend",
        ),
        (
            "DEFINE VIEW v2 MATERIALIZED AS SELECT * FROM orders WHERE n IN (SELECT n FROM people);",
            "depend",
        ),
        (
            "DEFINE VIEW v3 MATERIALIZED AS SELECT * FROM orders FETCH owner;",
            "FETCH",
        ),
        (
            "DEFINE VIEW v4 MATERIALIZED AS SELECT * FROM orders JOIN people ON orders.n = people.n;",
            "one table",
        ),
    ] {
        let refused = refusal(&mut session, script);
        assert!(
            matches!(&refused, Error::MaterializedShape { what: said, .. } if said.contains(what)),
            "`{script}` refused as {refused:?}"
        );
    }
    session
        .run("DEFINE VIEW plain AS SELECT * FROM orders;")
        .unwrap();
    assert!(matches!(
        refusal(
            &mut session,
            "DEFINE VIEW v5 MATERIALIZED AS SELECT * FROM plain;"
        ),
        Error::ViewIsNotATable { .. }
    ));
}

#[test]
fn nothing_but_its_maintainer_writes_a_kept_view() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW big MATERIALIZED AS {BIG};"))
        .unwrap();
    for script in [
        "CREATE big:1 = { n: 1 };",
        "DELETE big:1;",
        "UPDATE big:1 SET n = 2;",
    ] {
        assert!(
            matches!(refusal(&mut session, script), Error::ViewIsNotATable { .. }),
            "`{script}`"
        );
    }
}

#[test]
fn dropping_a_kept_view_takes_its_rows_and_its_state() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW big MATERIALIZED AS {BIG};"))
        .unwrap();
    session.run("DROP VIEW big;").unwrap();
    session
        .run("DEFINE VIEW big MATERIALIZED AS SELECT n FROM orders WHERE n < 3;")
        .unwrap();
    assert_eq!(records(&mut session, "SELECT * FROM big;").len(), 3);
    assert_eq!(maintain_views(&store).unwrap().views, 0);
}

#[test]
fn a_reader_who_may_see_only_part_of_the_source_is_refused() {
    let store = store();
    let mut owner = ready(&store);
    owner
        .run(&format!(
            "DEFINE VIEW big MATERIALIZED AS {BIG};\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';"
        ))
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", "correct horse battery").unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER bo ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         GRANT read ON orders FIELDS n TO ada;\n\
         GRANT read ON orders TO bo;",
    )
    .unwrap();
    let signed = |name: &str| {
        let mut session = Session::new(&store);
        session.sign_in(name, "correct horse battery").unwrap();
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        session
    };
    assert!(matches!(
        refusal(&mut signed("ada"), "SELECT * FROM big;"),
        Error::MaterializedFromHidden { .. }
    ));
    // The whole source is readable, so the view is.
    assert!(!records(&mut signed("bo"), "SELECT * FROM big;").is_empty());
}

#[test]
fn under_concurrent_writes_every_read_equals_the_view_at_its_version() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW big MATERIALIZED AS {BIG};"))
        .unwrap();
    std::thread::scope(|scope| {
        for writer in 0..2_i64 {
            let store = &store;
            scope.spawn(move || {
                let mut session = Session::new(store);
                session
                    .run("USE NAMESPACE prod; USE DATABASE shop;")
                    .unwrap();
                for step in 0..150_i64 {
                    let n = (step * 13 + writer * 7) % 60;
                    let script = if step % 5 == 4 {
                        format!("DELETE orders:{n};")
                    } else {
                        format!(
                            "UPSERT orders:{n} = {{ band: {}, total: {}, n: {n} }};",
                            n % 4,
                            (step * 11) % 50
                        )
                    };
                    // Contention is the caller's to retry; a lost step only
                    // changes what is written, never what must hold.
                    let _ = session.run(&script);
                }
            });
        }
        for _ in 0..20 {
            maintain_views(&store).unwrap();
            equals_its_read(&mut session, "big", BIG, true);
        }
    });
    maintain_views(&store).unwrap();
    equals_its_read(&mut session, "big", BIG, true);
    // Caught up, the stored rows are the read now.
    assert_eq!(
        records(&mut session, "SELECT * FROM big;"),
        records(&mut session, &format!("{BIG};"))
    );
}

#[test]
fn a_store_reopened_mid_maintenance_holds_its_view_at_its_version_and_catches_up() {
    let directory = tempfile::tempdir().unwrap();
    let open = || {
        let backend = tessari_lsm::LsmBackend::open(
            directory.path(),
            tessari_lsm::StoreConfig::new(tessari_lsm::Durability::ProcessCrashSafe),
        )
        .unwrap();
        Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
    };
    {
        let disk = open();
        let mut session = ready(&disk);
        session
            .run(&format!("DEFINE VIEW big MATERIALIZED AS {BIG};"))
            .unwrap();
        session
            .run("UPDATE orders:5 SET total = 1; DELETE orders:12;")
            .unwrap();
        // Ended with changes the view has not taken.
    }
    let disk = open();
    let mut session = Session::new(&disk);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    equals_its_read(&mut session, "big", BIG, true);
    maintain_views(&disk).unwrap();
    equals_its_read(&mut session, "big", BIG, true);
    assert_eq!(
        records(&mut session, "SELECT * FROM big;"),
        records(&mut session, &format!("{BIG};"))
    );
}
