//! A grouped materialized view kept one group at a time (ADR-0109 D2 amended,
//! Q-908): a change rewrites the rows of the groups it left and joined and no
//! other, and each row is stored under its group's key.
//!
//! The oracle is the one the rest of the views suite uses — the stored rows,
//! in order, are the view's read at the version the view states — run after
//! random batches that move records between groups, empty groups and make new
//! ones, over keys of several kinds.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use proptest::prelude::*;
use tessari_session::maintain_views;
use tessari_storage::{Catalog, RecordAddress, Store, Subject};
use tessari_types::{RecordId, Value};

use super::materialized_views::{equals_its_read, ready, records, store};

const BANDS: &str =
    "SELECT band, count(*) AS orders, sum(total) AS spent FROM orders GROUP BY band";

fn band(row: &Value) -> Option<&Value> {
    match row {
        Value::Object(fields) => fields.get("band"),
        _ => None,
    }
}

/// How many times the logs say a view row was written or removed — read from
/// the store, because the language answers history for tables, not views.
fn writes_of(store: &Store, view: &str, id: &RecordId) -> usize {
    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, view)
        .unwrap()
        .unwrap();
    transaction.rollback();
    let subject = Subject {
        namespace,
        database,
        table,
        id: id.clone(),
    };
    store
        .logs()
        .unwrap()
        .into_iter()
        .map(|log| {
            store
                .history_of(log, &subject, usize::MAX)
                .unwrap()
                .events
                .len()
        })
        .sum()
}

#[test]
fn a_change_rewrites_only_the_groups_it_touches() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW bands MATERIALIZED AS {BANDS};"))
        .unwrap();
    let before = records(&mut session, "SELECT * FROM bands;");
    assert_eq!(before.len(), 4);
    // orders:1 is in band 1 and stays there.
    session.run("UPDATE orders:1 SET total = 3;").unwrap();
    maintain_views(&store).unwrap();
    equals_its_read(&mut session, "bands", BANDS, false);
    for (id, row) in &before {
        let touched = band(row) == Some(&Value::from(1_i64));
        // Written once at definition; the touched group once more.
        let expected = if touched { 2 } else { 1 };
        assert_eq!(
            writes_of(&store, "bands", id),
            expected,
            "row {id} ({row:?})"
        );
    }
}

#[test]
fn a_group_row_keeps_its_identity_when_another_group_arrives() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW bands MATERIALIZED AS {BANDS};"))
        .unwrap();
    let before = records(&mut session, "SELECT * FROM bands;");
    // A band that sorts before every other one.
    session
        .run("CREATE orders:500 = { band: -1, total: 9, n: 500 };")
        .unwrap();
    maintain_views(&store).unwrap();
    equals_its_read(&mut session, "bands", BANDS, false);
    let after = records(&mut session, "SELECT * FROM bands;");
    assert_eq!(after.len(), 5);
    assert_eq!(band(&after[0].1), Some(&Value::from(-1_i64)));
    // Every earlier row is still there under the identity it had.
    for row in &before {
        assert!(after.contains(row), "{row:?} moved or changed");
    }
}

/// One write the random batches are made of.
#[derive(Debug, Clone)]
enum Step {
    /// Write order `n` whole, in a band of one of several kinds — or with no
    /// band at all.
    Put {
        n: u8,
        band: u8,
        total: u8,
    },
    Delete {
        n: u8,
    },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0_u8..30, 0_u8..7, 0_u8..50).prop_map(|(n, band, total)| Step::Put { n, band, total }),
        1 => (0_u8..30).prop_map(|n| Step::Delete { n }),
    ]
}

fn statement(step: &Step) -> String {
    match step {
        Step::Put { n, band, total } => {
            // Integers, a float equal to one of them, a string and an absent
            // field: groups whose keys cross kinds and number spellings.
            let band = match band {
                0..=2 => format!("band: {band}, "),
                3 => "band: 1.0, ".to_owned(),
                4 => "band: 'x', ".to_owned(),
                5 => "band: NULL, ".to_owned(),
                _ => String::new(),
            };
            format!(
                "UPSERT orders:{n} = {{ {band}kind: {}, total: {total}, n: {n} }};",
                n % 2
            )
        }
        Step::Delete { n } => format!("DELETE orders:{n};"),
    }
}

const PAIRS: &str = "SELECT band, kind, count(*) AS c, min(total) AS lo, max(total) AS hi, \
                     sum(total) AS s FROM orders WHERE total > 5 GROUP BY band, kind";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn a_grouped_view_equals_its_read_after_every_batch(
        batches in prop::collection::vec(prop::collection::vec(step(), 1..8), 1..6)
    ) {
        let store = store();
        let mut session = ready(&store);
        session
            .run(&format!(
                "DEFINE VIEW bands MATERIALIZED AS {BANDS};\n\
                 DEFINE VIEW pairs MATERIALIZED AS {PAIRS};"
            ))
            .unwrap();
        for batch in &batches {
            let script: String = batch.iter().map(statement).collect();
            session.run(&script).unwrap();
            maintain_views(&store).unwrap();
            equals_its_read(&mut session, "bands", BANDS, false);
            equals_its_read(&mut session, "pairs", PAIRS, false);
        }
        // Caught up, the stored rows are the read now — pinned to the read
        // itself rather than to another kept answer.
        prop_assert_eq!(
            records(&mut session, "SELECT * FROM pairs;")
                .into_iter()
                .map(|(_, row)| row)
                .collect::<Vec<_>>(),
            records(&mut session, &format!("{PAIRS};"))
                .into_iter()
                .map(|(_, row)| row)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn dropping_a_grouped_view_takes_its_membership_map() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW bands MATERIALIZED AS {BANDS};"))
        .unwrap();
    let view = {
        let mut transaction = store.begin().unwrap();
        let catalog = Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("prod").unwrap().unwrap();
        let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
        let view = catalog
            .table_id(namespace, database, "bands")
            .unwrap()
            .unwrap();
        // Pinned before the drop, so the absence after it is not vacuous.
        assert!(
            transaction
                .view_group_of(view, &RecordId::Int(0))
                .unwrap()
                .is_some()
        );
        transaction.rollback();
        view
    };
    session.run("DROP VIEW bands;").unwrap();
    let transaction = store.begin().unwrap();
    for n in 0..40 {
        assert_eq!(
            transaction.view_group_of(view, &RecordId::Int(n)).unwrap(),
            None
        );
    }
}

#[test]
fn a_grouped_view_kept_by_position_before_is_rebuilt_by_group_once() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(&format!("DEFINE VIEW bands MATERIALIZED AS {BANDS};"))
        .unwrap();
    // Put the view back the way a build before group keys left it: rows under
    // their positions, no membership map, a state that does not say by group.
    {
        let mut transaction = store.begin().unwrap();
        let catalog = Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("prod").unwrap().unwrap();
        let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
        let view = catalog
            .table_id(namespace, database, "bands")
            .unwrap()
            .unwrap();
        let rows = transaction.scan_table(namespace, database, view).unwrap();
        for (position, (id, payload)) in rows.into_iter().enumerate() {
            transaction.delete(RecordAddress::new(namespace, database, view, id));
            transaction.put(
                RecordAddress::new(
                    namespace,
                    database,
                    view,
                    RecordId::Int(i64::try_from(position).unwrap()),
                ),
                payload,
            );
        }
        transaction.forget_view_members(view).unwrap();
        let mut state = transaction.view_state(view).unwrap().unwrap();
        state.by_group = false;
        transaction.put_view_state(view, &state).unwrap();
        transaction.commit().unwrap();
    }
    // Nothing changed in the source, and the view is still rebuilt.
    maintain_views(&store).unwrap();
    let rows = records(&mut session, "SELECT * FROM bands;");
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|(id, _)| matches!(id, RecordId::Bytes(_))));
    equals_its_read(&mut session, "bands", BANDS, false);
    session.run("UPDATE orders:2 SET band = 0;").unwrap();
    maintain_views(&store).unwrap();
    assert_eq!(records(&mut session, "SELECT * FROM bands;").len(), 4);
    equals_its_read(&mut session, "bands", BANDS, false);
}
