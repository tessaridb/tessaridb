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
use bgv_db_types::Value;

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

/// A staff table with a field worth hiding, and ada granted only two of three.
fn field_scoped(store: &Store) {
    ready(store);
    let mut root = signed_in(store, "root");
    root.run(
        "DEFINE TABLE staff;\n\
         CREATE staff:1 = { name: 'ada', title: 'engineer', salary: 120000 };\n\
         CREATE staff:2 = { name: 'grace', title: 'admiral', salary: 200000 };",
    )
    .unwrap();
    root.run("GRANT read ON staff FIELDS name, title TO ada;")
        .unwrap();
}

#[test]
fn a_multi_valued_projection_reaches_only_into_the_record_the_session_may_see() {
    // A route reaching several values is a second way to read a field, so it is
    // a second way to leak one — and the check is that it needed **no** rule of
    // its own. A field permission edits the record before anything looks at it,
    // so the route reaches into a record the hidden field has already left, and
    // an unreadable field answers what any other empty reach answers.
    let store = store();
    ready(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE TABLE people;\n\
         CREATE people:1 = { name: 'ada', aliases: ['a.l.', 'countess'], \
                             secrets: ['a key', 'another'] };",
    )
    .unwrap();
    root.run("GRANT read ON people FIELDS name, aliases TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let outcomes = ada
        .run("SELECT aliases[*] AS shown, secrets[*] AS hidden FROM people:1;")
        .unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(
        format!("{:?}", fields.get("shown").unwrap()),
        r#"Array([String("a.l."), String("countess")])"#
    );
    assert_eq!(
        format!("{:?}", fields.get("hidden").unwrap()),
        "Array([])",
        "a multi-valued projection read past a field grant: {fields:?}"
    );

    // …and the owner, who may read it, gets the values — so the empty answer
    // above is the permission and not the projection failing to find anything.
    let outcomes = root
        .run("SELECT secrets[*] AS hidden FROM people:1;")
        .unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(
        format!("{:?}", fields.get("hidden").unwrap()),
        r#"Array([String("a key"), String("another")])"#
    );
}

#[test]
fn a_field_grant_hides_the_field_from_a_read() {
    let store = store();
    field_scoped(&store);
    let mut ada = signed_in(&store, "ada");
    let outcomes = ada.run("SELECT * FROM staff;").unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    assert_eq!(records.len(), 2);
    for (_, held) in records {
        let Value::Object(fields) = held else {
            panic!("not an object");
        };
        assert!(fields.contains_key("name"), "{fields:?}");
        assert!(!fields.contains_key("salary"), "{fields:?}");
    }
}

#[test]
fn a_condition_on_a_hidden_field_answers_nothing_rather_than_a_redacted_row() {
    // The difference between a permission and a redaction. If the field were
    // removed from the *answer* this would still return two rows, and the
    // condition would have reported on a field nobody may read.
    let store = store();
    field_scoped(&store);
    let mut ada = signed_in(&store, "ada");
    let outcomes = ada
        .run("SELECT * FROM staff WHERE salary > 100000;")
        .unwrap();
    assert!(outcomes.last().unwrap().records().unwrap().is_empty());
}

#[test]
fn the_bisection_attack_returns_zero() {
    // The statement this whole design exists to answer. It never shows `salary`
    // and asks about it precisely, so redacting the output would leave the count
    // intact and the field readable one bit at a time.
    let store = store();
    field_scoped(&store);
    let mut ada = signed_in(&store, "ada");
    for probe in [
        "SELECT count(*) AS n FROM staff WHERE salary > 100000;",
        "SELECT count(*) AS n FROM staff WHERE salary > 150000;",
        "SELECT count(*) AS n FROM staff WHERE salary = 200000;",
    ] {
        let outcomes = ada.run(probe).unwrap();
        let records = outcomes.last().unwrap().records().unwrap();
        // Every probe answers the same thing, which is what makes it useless:
        // no records matched, so the fold has nothing to fold and there is no
        // row — indistinguishable from a table holding nobody.
        assert!(records.is_empty(), "{probe} answered {records:?}");
    }

    // And the owner, who may read it, gets the real answers — otherwise the
    // assertions above pass for a store that simply does not work.
    let mut root = signed_in(&store, "root");
    let outcomes = root
        .run("SELECT count(*) AS n FROM staff WHERE salary > 100000;")
        .unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(row) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(row.get("n"), Some(&Value::from(2_i64)));
}

#[test]
fn an_index_on_a_hidden_field_changes_neither_answer() {
    // The governing rule of this store, at the place a permission could break
    // it: an index narrows and never answers, so the candidates it offers are
    // re-tested against the record the reader may actually see.
    let store = store();
    field_scoped(&store);
    signed_in(&store, "root")
        .run("DEFINE INDEX by_salary ON staff FIELDS salary;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let outcomes = ada
        .run("SELECT * FROM staff WHERE salary = 120000;")
        .unwrap();
    assert!(outcomes.last().unwrap().records().unwrap().is_empty());
}

#[test]
fn ordering_by_a_hidden_field_leaks_no_ordering() {
    let store = store();
    field_scoped(&store);
    let mut ada = signed_in(&store, "ada");
    let outcomes = ada
        .run("SELECT * FROM staff ORDER BY salary DESC;")
        .unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    // Every key is `NONE`, so the sort is a no-op and the answer is the read's
    // own order — which says nothing about salaries.
    assert_eq!(records.len(), 2);
    for (_, held) in records {
        let Value::Object(fields) = held else {
            panic!("not an object");
        };
        assert!(!fields.contains_key("salary"));
    }
}

#[test]
fn a_join_hides_the_field_on_the_side_that_declared_it() {
    let store = store();
    field_scoped(&store);
    let mut root = signed_in(&store, "root");
    root.run("CREATE users:2 = { name: 'grace' };").unwrap();
    root.run("GRANT read ON users TO ada;").unwrap();

    let mut ada = signed_in(&store, "ada");
    let outcomes = ada
        .run("SELECT * FROM users JOIN staff ON users.name = staff.name;")
        .unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    assert!(!records.is_empty());
    for (_, held) in records {
        let Value::Object(row) = held else {
            panic!("not an object");
        };
        let Some(Value::Object(side)) = row.get("staff") else {
            panic!("no staff side");
        };
        assert!(!side.contains_key("salary"), "{side:?}");
    }
}

#[test]
fn fetch_into_a_field_scoped_table_hides_it_there_too() {
    // A reference is an address into a *different* table, and following one is
    // not a way to read what that table's grant refuses.
    let store = store();
    field_scoped(&store);
    let mut root = signed_in(&store, "root");
    root.run("DEFINE TABLE notes; CREATE notes:1 = { about: staff:1 };")
        .unwrap();
    root.run("GRANT read ON notes TO ada;").unwrap();

    let mut ada = signed_in(&store, "ada");
    let outcomes = ada.run("SELECT * FROM notes FETCH about;").unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(row) = &records[0].1 else {
        panic!("not an object");
    };
    let Some(Value::Object(about)) = row.get("about") else {
        panic!("the reference was not followed: {row:?}");
    };
    assert!(about.contains_key("name"), "{about:?}");
    assert!(!about.contains_key("salary"), "{about:?}");
}

#[test]
fn a_grant_naming_no_fields_still_covers_the_whole_record() {
    let store = store();
    field_scoped(&store);
    signed_in(&store, "root")
        .run("GRANT read ON staff TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let outcomes = ada.run("SELECT * FROM staff:1;").unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object");
    };
    assert!(fields.contains_key("salary"), "{fields:?}");
}

#[test]
fn fields_with_a_write_is_refused_with_its_reason() {
    // A user who cannot see a field but may write the record would overwrite it
    // whole and destroy what they cannot see.
    let store = store();
    ready(&store);
    let refused = signed_in(&store, "root")
        .run("GRANT read, write ON users FIELDS name TO ada;")
        .expect_err("a refusal");
    assert!(refused.to_string().contains("destroy"), "{refused}");
}
