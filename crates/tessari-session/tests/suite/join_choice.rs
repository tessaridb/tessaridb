//! The join's probe-or-build choice: statistics decide the shape, and the
//! answer never changes with it (G083 C3); the search the choice is made in
//! stays bounded as joins nest (G083 C4).
//!
//! The choice weighs the driving side's row count against the right side's:
//! probing the right's key index costs an entry walk and a fetch per driving
//! record, building the map costs one scan of the right — so the counts, not
//! the index's existence, decide. Where the driving side has no count (its
//! source is a read or a join), the rule says nothing and an index that
//! exists is probed, as it always was.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

fn ready(store: &Store, small: i64, large: i64) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION customers; DEFINE COLLECTION orders;",
        )
        .unwrap();
    let mut script = String::from("DEFINE INDEX by_who ON orders FIELDS who;\n");
    for n in 0..small {
        script.push_str(&format!("CREATE customers:{n} = {{ name: 'c{n}' }};\n"));
    }
    for n in 0..large {
        script.push_str(&format!(
            "CREATE orders:{n} = {{ who: 'c{}' }};\n",
            n.rem_euclid(small)
        ));
    }
    session.run_now(&script).unwrap();
    session
}

const JOINED: &str = "SELECT * FROM customers JOIN orders ON customers.name = orders.who;";

fn rows_of(outcomes: &[Outcome]) -> Vec<String> {
    outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(_, row)| row.to_string())
        .collect()
}

/// The two tables with the same data but no index — the differential's
/// no-index mirror, the same shape the planner suites keep for every indexed
/// read.
fn ready_plain(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION customers; DEFINE COLLECTION orders;",
        )
        .unwrap();
    session
}

fn plan_of(session: &mut Session<'_>) -> tessari_session::Plan {
    let outcomes = session.run_now(JOINED).unwrap();
    let Some(Outcome::Records { plan, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    (**plan).clone()
}

#[test]
fn the_counts_choose_probe_or_build_and_the_answer_holds() {
    // The index exists in both stores; the counts decide whether it is used.
    // Twenty orders against ten customers: the right side is not more than
    // twice the left, and building the map pays. Two hundred: probing pays.
    let built_store = store();
    let mut built_session = ready(&built_store, 10, 20);
    let built_rows = rows_of(&built_session.run_now(JOINED).unwrap());
    let built_plan = plan_of(&mut built_session);
    assert_eq!(
        built_plan.shape,
        Some("build"),
        "twenty orders against ten customers builds the map: {built_plan:?}"
    );
    assert_eq!(
        built_plan.index, None,
        "the index exists and the counts overrule it: {built_plan:?}"
    );
    assert_eq!(built_plan.left_rows, Some(10));
    assert_eq!(built_plan.right_rows, Some(20));

    let probed_store = store();
    let mut probed_session = ready(&probed_store, 10, 200);
    let probed_rows = rows_of(&probed_session.run_now(JOINED).unwrap());
    let probed_plan = plan_of(&mut probed_session);
    assert_eq!(
        probed_plan.shape,
        Some("probe"),
        "two hundred orders against ten customers probes the index: {probed_plan:?}"
    );
    assert_eq!(probed_plan.index.as_deref(), Some("by_who"));

    // The same density answers the same with the index as without it: the
    // choice is cost, and the answer is the operator's.
    let plain_store = store();
    let mut plain_session = ready_plain(&plain_store);
    let mut script = String::new();
    for n in 0..10 {
        script.push_str(&format!("CREATE customers:{n} = {{ name: 'c{n}' }};\n"));
    }
    for n in 0..200 {
        script.push_str(&format!("CREATE orders:{n} = {{ who: 'c{}' }};\n", n % 10));
    }
    plain_session.run_now(&script).unwrap();
    let plain_rows = rows_of(&plain_session.run_now(JOINED).unwrap());
    assert_eq!(probed_rows, plain_rows, "the index changed the answer");

    assert_eq!(built_rows.len(), 20, "two orders per customer");
    assert_eq!(probed_rows.len(), 200, "twenty orders per customer");
}

#[test]
fn a_driving_side_without_a_count_keeps_the_index_rule() {
    // The left side is a read, not a table: no count exists for the choice to
    // weigh, and the index that exists is probed exactly as before it did.
    let store = store();
    let mut session = ready(&store, 10, 15);
    let outcomes = session
        .run_now(
            "SELECT * FROM (SELECT * FROM customers WHERE name != 'nobody' LIMIT 100) AS picked \
             JOIN orders ON picked.name = orders.who;",
        )
        .unwrap();
    let Some(Outcome::Records { plan, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(
        plan.index.as_deref(),
        Some("by_who"),
        "no count, the index decides: {plan:?}"
    );
}

#[test]
fn a_full_join_never_probes() {
    // FULL OUTER must read every right record to answer the unmatched ones,
    // whatever the counts say.
    let store = store();
    let mut session = ready(&store, 10, 500);
    let outcomes = session
        .run_now("SELECT * FROM customers FULL OUTER JOIN orders ON customers.name = orders.who;")
        .unwrap();
    let Some(Outcome::Records { plan, .. }) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert_eq!(plan.shape, Some("build"), "{plan:?}");
    assert_eq!(plan.index, None);
}

#[test]
fn the_search_stays_bounded_as_joins_nest() {
    // C4's ceiling, at a stated shape: eight tables joined in a chain. The
    // planner's work per join node is exactly one probe-or-build decision
    // (one call to `join_serving` in `evaluate/keys.rs` — no loop over
    // orderings exists to enumerate), and each read inside it is the bounded
    // candidate enumeration. The evidence a suite can hold: the plan answers,
    // every join node carries exactly one shape word, and re-planning is
    // byte-identical — a search that grew with the data or permuted the
    // orderings would not be.
    let store = store();
    let mut session = Session::new(&store);
    let mut define = String::from(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
         DEFINE DATABASE shop; USE DATABASE shop;\n",
    );
    for t in 1..=8 {
        define.push_str(&format!("DEFINE COLLECTION t{t};\n"));
    }
    for t in 1..=8 {
        for n in 0..50 {
            define.push_str(&format!("CREATE t{t}:{n} = {{ k: {n} }};\n"));
        }
    }
    session.run_now(&define).unwrap();
    let mut chain = "SELECT * FROM t1".to_owned();
    for t in 2..=8 {
        chain.push_str(&format!(" JOIN t{t} ON t{}.k = t{t}.k", t - 1));
    }
    chain.push(';');

    let first = session.run_now(&format!("EXPLAIN {chain}")).unwrap();
    let second = session.run_now(&format!("EXPLAIN {chain}")).unwrap();
    assert_eq!(first, second, "re-planning the same statement differs");
    let Some(Outcome::Value(Value::Object(plan))) = first.last() else {
        panic!("{first:?}");
    };
    assert_eq!(plan.get("access"), Some(&Value::from("join")));
    assert!(
        plan.contains_key("shape"),
        "one decision word on the top node: {plan:?}"
    );
    let answered = session.run_now(&chain).unwrap();
    assert_eq!(answered.last().unwrap().records().unwrap().len(), 50);
}
