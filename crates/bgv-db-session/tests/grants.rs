//! What a grant does to a session that has one.
//!
//! The corpus pins the shape of the statements and the refusals an anonymous
//! session reaches. Everything below needs a session that can **sign in**, which
//! a corpus file cannot do — the same reason the per-role checks live outside it.
//!
//! The rule under test is one sentence: **a user's grants, if they have any, are
//! the whole story, and a user with none is governed by their role.**

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::Session;
use bgv_db_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

const PASSWORD: &str = "correct horse battery";

/// Two tables, a record in each, and an editor scoped to the database.
///
/// The store is open until the `DEFINE USER`, so everything before it runs
/// anonymously — which is what makes an empty store usable at all.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users; DEFINE TABLE orders;\n\
             CREATE users:1 = { name: 'ada' };\n\
             CREATE orders:1 = { total: 3 };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    // The first user closed the store, so the second is declared by the owner —
    // which is the shape every store past its first day is in.
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
    session
}

/// A signed-in session with the tenancy selected.
fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn a_user_with_no_grants_keeps_their_roles_access() {
    // The property that lets this feature exist at all: adding grants to the
    // store changed nothing for anybody who has none.
    let store = store();
    ready(&store);
    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    ada.run("SELECT * FROM orders;").unwrap();
    ada.run("CREATE orders:2 = { total: 9 };").unwrap();
}

#[test]
fn the_first_grant_is_also_a_restriction() {
    // Which is the whole point. A role can only widen; if a grant merely added
    // to one, nothing could ever be narrowed and the feature would be
    // decoration.
    let store = store();
    ready(&store);
    signed_in(&store, "root")
        .run("GRANT read ON users TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    let refused = ada
        .run("SELECT * FROM orders;")
        .expect_err("orders was never granted");
    let said = refused.to_string();
    assert!(said.contains("ada"), "{said}");
    assert!(said.contains("orders"), "{said}");
}

#[test]
fn a_read_grant_does_not_carry_a_write() {
    let store = store();
    ready(&store);
    signed_in(&store, "root")
        .run("GRANT read ON users TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    let refused = ada
        .run("CREATE users:2 = { name: 'grace' };")
        .expect_err("only read was granted");
    assert!(refused.to_string().contains("write"), "{refused}");
}

#[test]
fn granting_twice_is_one_grant_and_granting_replaces_what_was_there() {
    // Identified by its pair rather than by an allocated id, so a provisioning
    // script that runs twice does not build up state nobody meant — and a grant
    // says what the result is rather than what it adds.
    let store = store();
    ready(&store);
    let mut root = signed_in(&store, "root");
    root.run("GRANT read, write ON users TO ada;").unwrap();
    root.run("GRANT read, write ON users TO ada;").unwrap();
    // Narrowing by re-granting, which is the widening story in reverse.
    root.run("GRANT read ON users TO ada;").unwrap();

    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    assert!(ada.run("CREATE users:3 = { name: 'x' };").is_err());

    // One grant, so it is still the last one and cannot be revoked.
    assert!(
        root.run("REVOKE read ON users FROM ada;").is_err(),
        "two grants accumulated where there should be one"
    );
}

#[test]
fn revoking_a_verb_leaves_the_rest_of_the_grant() {
    let store = store();
    ready(&store);
    let mut root = signed_in(&store, "root");
    root.run("GRANT read, write ON users TO ada;").unwrap();
    root.run("GRANT read ON orders TO ada;").unwrap();
    root.run("REVOKE write ON users FROM ada;").unwrap();

    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    assert!(ada.run("CREATE users:4 = { name: 'x' };").is_err());
}

#[test]
fn revoking_the_last_grant_is_refused_because_it_would_widen() {
    // The one direction a revocation must never go silently: from a named table
    // to every table the role allows.
    let store = store();
    ready(&store);
    let mut root = signed_in(&store, "root");
    root.run("GRANT read ON users TO ada;").unwrap();
    let refused = root
        .run("REVOKE read ON users FROM ada;")
        .expect_err("the last grant");
    let said = refused.to_string();
    assert!(said.contains("widen"), "{said}");

    // And ada is still exactly as scoped as she was.
    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    assert!(ada.run("SELECT * FROM orders;").is_err());
}

#[test]
fn only_an_owner_may_grant_or_revoke() {
    // Granting is administering, which is the same rule `DEFINE USER` follows.
    let store = store();
    ready(&store);
    let mut ada = signed_in(&store, "ada");
    assert!(ada.run("GRANT read ON users TO ada;").is_err());
    assert!(ada.run("REVOKE read ON users FROM ada;").is_err());
}

#[test]
fn a_grant_governed_user_cannot_declare_a_table_and_is_told_why() {
    // A grant names a table that exists, so declaring one has no grant that
    // could permit it. Saying so beats a refusal that reads like a bug.
    let store = store();
    ready(&store);
    signed_in(&store, "root")
        .run("GRANT read, write ON users TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let refused = ada.run("DEFINE TABLE invoices;").expect_err("a refusal");
    let said = refused.to_string();
    assert!(said.contains("governed by grants"), "{said}");
}

#[test]
fn a_grant_reaches_every_table_a_statement_names_and_not_only_the_first() {
    // `RELATE` names three: both endpoints and the edge table. A grant on the
    // edge alone would let somebody write a link between records they cannot
    // see, which is a way around the grant rather than a use of it.
    let store = store();
    ready(&store);
    let mut root = signed_in(&store, "root");
    root.run("DEFINE TABLE follows EDGE;").unwrap();
    root.run("GRANT read, write ON follows TO ada;").unwrap();

    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("RELATE users:1->follows->users:1;")
        .expect_err("users was not granted");
    assert!(refused.to_string().contains("users"), "{refused}");
}

#[test]
fn a_join_needs_a_grant_on_both_sides() {
    // The newest way to reach a table, and the reason `tables_named` matches
    // exhaustively rather than falling through to a wildcard.
    let store = store();
    ready(&store);
    signed_in(&store, "root")
        .run("GRANT read ON users TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("SELECT * FROM users JOIN orders ON users.name = orders.total;")
        .expect_err("orders was not granted");
    assert!(refused.to_string().contains("orders"), "{refused}");
}

#[test]
fn grants_survive_a_reopen_because_they_are_catalog_and_not_memory() {
    // Somewhere to put a per-object list, which is what the node asked for. A
    // second session over the same store reads what the first one wrote,
    // because a grant is an ordinary record in the system tenancy.
    let store = store();
    ready(&store);
    signed_in(&store, "root")
        .run("GRANT read ON users TO ada;")
        .unwrap();

    let mut later = Session::new(&store);
    later.sign_in("ada", PASSWORD).unwrap();
    later.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    later.run("SELECT * FROM users;").unwrap();
    assert!(later.run("SELECT * FROM orders;").is_err());
}

#[test]
fn dropping_a_user_takes_their_grants_with_them() {
    // A later user allocated the same id would otherwise inherit permissions
    // nobody gave them — the same shape of bug as a reused table id resolving a
    // stale reference.
    let store = store();
    ready(&store);
    let mut root = signed_in(&store, "root");
    root.run("GRANT read ON users TO ada;").unwrap();
    root.run("DROP USER ada;").unwrap();
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();

    // A fresh ada has no grants, so her role governs and both tables are hers.
    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM users;").unwrap();
    ada.run("SELECT * FROM orders;").unwrap();
}
