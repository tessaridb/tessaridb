//! The four rules the authority model was asked for, each as a script that runs.
//!
//! These are acceptance tests for a *vocabulary*, not for enforcement. What they
//! assert is that each rule can now be **said** and **read back** — that the
//! store holds the set the statement described and no more. What a held set
//! causes the store to refuse is decided one wave later, and asserting it here
//! would be asserting against code that does not exist yet.
//!
//! The rules, in the words they were given in:
//!
//! 1. the highest authority runs the server and the cluster;
//! 2. cluster management can stand on its own;
//! 3. a namespace has an authority that creates and drops databases in it;
//! 4. reading or writing inside a namespace confers **neither** of those.
//!
//! The fourth is the one no ladder could express, and it is the reason the model
//! is a set of `(kind, reach)` pairs rather than a rank: `write` and `manage`
//! have to be independent in both directions, and in a total order they cannot
//! be — put `manage` above `write` and every manager writes, put it below and
//! every writer manages.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two namespaces, a database in each, and a store owner to declare the rest.
///
/// Two namespaces rather than one so that a reach can be *wrong* rather than
/// merely absent: an authority over `prod` that also answered for `staging`
/// would pass every single-namespace test ever written.
fn governed(store: &Store) {
    let mut opening = Session::new(store);
    opening
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;\n\
             DEFINE NAMESPACE staging; USE NAMESPACE staging;\n\
             DEFINE DATABASE sandbox;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
}

/// A signed-in session, with no tenancy selected.
fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// What `INFO FOR USER` says a user holds, as `kind@reach` strings.
///
/// Read through the statement rather than out of the catalog on purpose: with
/// enforcement a wave away, this report is the *only* observable effect a grant
/// has, so a test that reached past it would pass over a grant nobody could see.
fn held(session: &mut Session<'_>, user: &str) -> Vec<String> {
    let outcomes = session.run(&format!("INFO FOR USER {user};")).unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report, got {outcomes:?}");
    };
    let Some(Value::Array(authorities)) = report.get("authorities") else {
        panic!("expected an authority list, got {report:?}");
    };
    let mut written: Vec<String> = authorities
        .iter()
        .map(|held| {
            let Value::Object(one) = held else {
                panic!("expected an object per authority");
            };
            match (one.get("authority"), one.get("reach")) {
                (Some(Value::String(kind)), Some(Value::String(reach))) => {
                    format!("{kind}@{reach}")
                }
                other => panic!("expected a kind at a reach, found {other:?}"),
            }
        })
        .collect();
    written.sort();
    written
}

#[test]
fn the_highest_authority_holds_every_kind_over_the_whole_store() {
    // Rule 1. And the shape of the answer matters as much as its content: the
    // top is five ordinary authorities at store reach, not an `is_root` branch.
    // A privileged branch is how a model acquires a path its negative tests
    // never cover, because there is nothing there to write a negative test
    // against.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    assert_eq!(
        held(&mut root, "root"),
        vec![
            "govern@store".to_owned(),
            "manage@store".to_owned(),
            "operate@store".to_owned(),
            "read@store".to_owned(),
            "write@store".to_owned(),
        ]
    );
}

#[test]
fn running_the_cluster_can_stand_alone() {
    // Rule 2. `operate` over the store and nothing else: this user is trusted
    // with topology, replicas and the backup file, and with none of the data
    // those things move around. No role names this set — `owner` would hand
    // them every record in the store — so before there were authorities it was
    // not a thing anybody could be.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("DEFINE USER ops AUTHORITIES operate PASSWORD 'correct horse battery';")
        .unwrap();

    let mut root = signed_in(&store, "root");
    assert_eq!(held(&mut root, "ops"), vec!["operate@store".to_owned()]);
}

#[test]
fn a_namespace_authority_reaches_that_namespace_and_not_its_neighbour() {
    // Rule 3, and the half of it that is easy to get wrong. `manage` over
    // `prod` is what creates and drops databases in `prod` — and the assertion
    // that matters is the *absence* of `staging`, because an authority that
    // quietly reached every namespace would satisfy a test that only looked at
    // the one it was granted on.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("DEFINE USER nadia ON NAMESPACE prod AUTHORITIES manage PASSWORD 'correct horse battery';")
        .unwrap();

    let mut root = signed_in(&store, "root");
    assert_eq!(held(&mut root, "nadia"), vec!["manage@prod".to_owned()]);
}

#[test]
fn reading_and_writing_a_namespace_confers_neither_creating_nor_dropping() {
    // Rule 4 — the one a ladder could not hold at any position, and therefore
    // the reason this model exists at all.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER wilma ON NAMESPACE prod AUTHORITIES read, write \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut root = signed_in(&store, "root");
    let holding = held(&mut root, "wilma");
    assert_eq!(
        holding,
        vec!["read@prod".to_owned(), "write@prod".to_owned()]
    );
    assert!(
        !holding.contains(&"manage@prod".to_owned()),
        "writing a namespace's records must not confer creating databases in it"
    );
}

#[test]
fn a_grant_adds_to_what_is_held_and_a_revocation_takes_only_what_it_names() {
    // The vocabulary's other half. A grant that *replaced* would make every
    // grant a silent revocation of every other one a user holds, which is the
    // failure that looks like nothing until the day somebody needs the second.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE USER kim ON NAMESPACE prod AUTHORITIES read PASSWORD 'correct horse battery';",
    )
    .unwrap();

    root.run("GRANT manage ON DATABASE prod.shop TO kim;")
        .unwrap();
    assert_eq!(
        held(&mut root, "kim"),
        vec!["manage@prod.shop".to_owned(), "read@prod".to_owned()],
        "a grant must add to what is held rather than replace it"
    );

    root.run("REVOKE read ON NAMESPACE prod FROM kim;").unwrap();
    assert_eq!(
        held(&mut root, "kim"),
        vec!["manage@prod.shop".to_owned()],
        "a revocation must take exactly what it names"
    );

    // Removing something nobody holds is not an error: the statement asks for a
    // user without it, and a user without it is what it leaves. A revocation
    // that failed halfway down a list would be worse than an idempotent one.
    root.run("REVOKE operate ON STORE FROM kim;").unwrap();
    assert_eq!(held(&mut root, "kim"), vec!["manage@prod.shop".to_owned()]);
}

#[test]
fn a_role_and_the_set_it_names_are_the_same_declaration() {
    // What keeps the number of role *names* at three while the number of
    // expressible sets is the whole lattice: `ROLE` is sugar, and it has to
    // produce exactly what spelling the set out produces or the two spellings
    // are two features.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE USER byrole ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER byset ON DATABASE prod.shop AUTHORITIES read, write, manage \
         PASSWORD 'correct horse battery';",
    )
    .unwrap();

    assert_eq!(held(&mut root, "byrole"), held(&mut root, "byset"));
}

#[test]
fn a_set_no_role_describes_is_reported_without_one() {
    // The honest half of keeping `role` in the record. It is written so that a
    // binary predating the authority set reads *something*, and it must never
    // read something wider than the truth — so a set no role fits carries no
    // role at all rather than the nearest one. `viewer` here would hand an older
    // binary a read this user does not hold.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE USER nadia ON NAMESPACE prod AUTHORITIES manage PASSWORD 'correct horse battery';",
    )
    .unwrap();

    let outcomes = root.run("INFO FOR USER nadia;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report");
    };
    assert!(
        !report.contains_key("role"),
        "a set no role describes must not be reported as a role: {report:?}"
    );

    // And the case that does fit still carries one, so the absence above is the
    // rule working rather than the field having been dropped.
    root.run("DEFINE USER seen ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .unwrap();
    let outcomes = root.run("INFO FOR USER seen;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report");
    };
    assert_eq!(report.get("role"), Some(&Value::from("viewer")));
}

#[test]
fn a_reach_is_named_by_keyword_so_a_table_can_never_be_read_as_one() {
    // The ambiguity that had to be made unrepresentable rather than resolved.
    // Before `STORE` was reserved, `GRANT read ON store TO ada` was a valid
    // statement meaning the *table* `store`; a reach spelled as a bare word
    // would have silently widened it to every namespace. Reserving the word
    // costs a table the name, and that cost is asserted here so it is a decision
    // on the record rather than a surprise in a release note.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    let refused = root
        .run("DEFINE TABLE store;")
        .expect_err("`store` is a reserved word");
    // The refusal names the keyword as the lexer spells it, which is the
    // evidence that the word was taken as a keyword rather than rejected for
    // some unrelated reason.
    assert!(refused.to_string().contains("STORE"), "{refused}");
}
