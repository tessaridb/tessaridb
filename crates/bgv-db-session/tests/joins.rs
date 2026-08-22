//! Matching two tables on a value neither of them stores a pointer for.
//!
//! `FETCH` is the join a *stored* relationship has: a reference is an address,
//! so following one is a point read. This is the other kind, where nobody wrote
//! an address down and the value is all the two sides share.
//!
//! The corpus covers what a join answers. These cover the two things a row count
//! cannot see: **which access path ran**, and that it is the same answer either
//! way — an index in this store may change what a read costs and never what it
//! returns, and a join is the newest place that rule has to hold.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{AccessPath, Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two users and three orders, one of the orders for nobody.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users; DEFINE TABLE orders;\n\
             CREATE users:1 = { name: 'ada', city: 'london' };\n\
             CREATE users:2 = { name: 'grace', city: 'new york' };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };\n\
             CREATE orders:2 = { who: 'ada', total: 7 };\n\
             CREATE orders:3 = { who: 'nobody', total: 1 };",
        )
        .unwrap();
    session
}

const JOINED: &str = "SELECT * FROM users JOIN orders ON users.name = orders.who;";

/// The rows and the path one statement answered with.
fn answered(session: &mut Session<'_>, script: &str) -> (Vec<(RecordId, Value)>, AccessPath) {
    let outcomes = session.run(script).unwrap();
    let last = outcomes.last().unwrap();
    match last {
        Outcome::Records { records, path } => (records.clone(), *path),
        other => panic!("not records: {other:?}"),
    }
}

#[test]
fn a_row_is_a_record_with_two_named_sides() {
    // The naming decision, which is the decision to have no naming rule: nothing
    // is merged, so `users.name` and `orders.who` were never in danger of
    // colliding and no alias syntax had to be invented.
    let store = store();
    let mut session = ready(&store);
    let (rows, _) = answered(&mut session, JOINED);
    assert_eq!(rows.len(), 2);

    let Value::Object(row) = &rows[0].1 else {
        panic!("not an object: {:?}", rows[0].1);
    };
    assert_eq!(row.len(), 2, "a row holds exactly its two sides");
    let Some(Value::Object(user)) = row.get("users") else {
        panic!("no `users` side: {row:?}");
    };
    let Some(Value::Object(order)) = row.get("orders") else {
        panic!("no `orders` side: {row:?}");
    };
    assert_eq!(user.get("name"), Some(&Value::from("ada")));
    assert!(order.contains_key("total"));
}

#[test]
fn one_left_record_matching_twice_answers_twice_under_one_identity() {
    // The cost of keying answers by record: a row is not a record, and two rows
    // from one left record carry one id. Asserted rather than left to be found.
    let store = store();
    let mut session = ready(&store);
    let (rows, _) = answered(&mut session, JOINED);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, rows[1].0, "both rows came from users:1");
}

#[test]
fn an_index_on_the_right_key_changes_the_path_and_not_the_answer() {
    // The rule this whole store is built to keep, at the newest place it has to
    // hold. The statements are identical; only the index between them differs.
    let store = store();
    let mut session = ready(&store);
    let (scanned, path) = answered(&mut session, JOINED);
    assert_eq!(path, AccessPath::Scan);

    session
        .run("DEFINE INDEX by_who ON orders FIELDS who;")
        .unwrap();
    let (indexed, path) = answered(&mut session, JOINED);
    assert_eq!(path, AccessPath::Index, "the index was not used");
    assert_eq!(scanned, indexed, "the index changed the answer");
}

#[test]
fn the_join_agrees_with_the_operator_it_is_spelled_with() {
    // `Number`'s equality *is* its order, so `3` and `3.0` are equal — which a
    // hash-keyed join would miss silently, since `Value` derives `Hash`
    // structurally while its equality is not structural.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE users:3 = { name: 'exact', qty: 3 };\n\
             CREATE orders:4 = { who: 'exact', qty: 3.0 };",
        )
        .unwrap();

    let across = "SELECT * FROM users JOIN orders ON users.qty = orders.qty;";
    let (rows, _) = answered(&mut session, across);
    assert_eq!(rows.len(), 1, "an integer did not match an equal float");

    // And the predicate it is spelled with agrees, which is the actual claim.
    let (same, _) = answered(&mut session, "SELECT * FROM orders WHERE qty = 3;");
    assert_eq!(same.len(), 1, "`= 3` did not match the float either");

    // Through the index too, whose encoding normalises for its own reasons.
    session
        .run("DEFINE INDEX by_qty ON orders FIELDS qty;")
        .unwrap();
    let (indexed, path) = answered(&mut session, across);
    assert_eq!(path, AccessPath::Index);
    assert_eq!(rows, indexed);
}

#[test]
fn the_two_sides_of_on_may_be_written_either_way_round() {
    // A reader writing the condition is thinking about the two fields, not about
    // which table the statement happened to name first.
    let store = store();
    let mut session = ready(&store);
    let (forwards, _) = answered(&mut session, JOINED);
    let (backwards, _) = answered(
        &mut session,
        "SELECT * FROM users JOIN orders ON orders.who = users.name;",
    );
    assert_eq!(forwards, backwards);
}

#[test]
fn a_where_and_an_ordering_read_the_composite() {
    let store = store();
    let mut session = ready(&store);
    let (rows, _) = answered(
        &mut session,
        "SELECT * FROM users JOIN orders ON users.name = orders.who \
         WHERE orders.total > 5 ORDER BY orders.total DESC;",
    );
    assert_eq!(rows.len(), 1);
    let Value::Object(row) = &rows[0].1 else {
        panic!("not an object");
    };
    let Some(Value::Object(order)) = row.get("orders") else {
        panic!("no `orders` side");
    };
    assert_eq!(order.get("total"), Some(&Value::from(7_i64)));
}

#[test]
fn a_join_of_a_table_to_itself_is_refused_with_its_reason() {
    // Two records under one name is not a row anybody can read, and the fix is
    // aliases — a language surface rather than a clause.
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run("SELECT * FROM users JOIN users ON users.name = users.city;")
        .expect_err("a refusal");
    assert!(refused.to_string().contains("users"), "{refused}");
}

#[test]
fn an_on_naming_a_third_table_says_which_two_it_could_have_named() {
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run("SELECT * FROM users JOIN orders ON users.name = customers.who;")
        .expect_err("a refusal");
    let said = refused.to_string();
    assert!(said.contains("customers"), "{said}");
    assert!(said.contains("users") && said.contains("orders"), "{said}");
}

#[test]
fn a_join_cannot_reach_a_table_outside_the_session_users_tenancy() {
    // A join is a new path that reaches records, and the last time this store
    // grew one of those it grew two authorization holes with it. This one is
    // closed by construction rather than by a check written here: both sides go
    // through `resolve_table`, which resolves a tenancy, and the tenancy check
    // lives at that resolution precisely so that every path reaching a record
    // passes it. Asserted rather than reasoned about.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE DATABASE elsewhere; USE DATABASE elsewhere;\n\
             DEFINE TABLE secrets; CREATE secrets:1 = { who: 'ada' };\n\
             USE DATABASE shop;\n\
             DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse';",
        )
        .unwrap();

    // Signed in first: the store is closed the moment it has a user, so even a
    // `USE` needs an identity.
    let mut scoped = Session::new(&store);
    scoped.sign_in("ada", "correct horse").unwrap();
    scoped
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();

    // The join it is allowed to run still works.
    let allowed = scoped.run(JOINED).unwrap();
    assert!(!allowed.is_empty());

    // And the one reaching into the other database does not.
    let refused = scoped
        .run("SELECT * FROM users JOIN elsewhere.secrets ON users.name = secrets.who;")
        .expect_err("a refusal");
    assert!(refused.to_string().contains("elsewhere"), "{refused}");
}
