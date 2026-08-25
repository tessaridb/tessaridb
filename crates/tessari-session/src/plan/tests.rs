//! The planner end to end — that enumeration puts the right ceiling on each
//! kind of candidate, and that the ranking then chooses on it.
//!
//! Its own module rather than a block inside one part, because it reaches
//! across all of them: a perfect ranking rule fed a wrong `Rows` chooses
//! wrongly and quietly, so neither half can be tested alone and claim the
//! planner works.

#![allow(clippy::panic)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::{Expr, Source, StatementKind, parse};
use tessari_storage::{Catalog, Store};

use crate::search::Searched;
use crate::session::Session;

use super::candidate::Rows;
use super::rank::choose;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).expect("a store")
}

/// `users` with a unique index on `email`, a secondary one on `city`, and a
/// search index over an analysed `body`.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE ANALYZER simple FILTERS lowercase;\n\
             DEFINE TABLE users;\n\
             DEFINE FIELD body ON users TYPE string ANALYZER simple;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
             DEFINE INDEX by_city ON users FIELDS city;\n\
             DEFINE INDEX by_name ON users FIELDS name;\n\
             DEFINE INDEX by_body ON users FIELDS body SEARCH;\n\
             CREATE users:1 = { email: 'a@x', city: 'london', name: 'ada', body: 'lock' };\n\
             CREATE users:2 = { email: 'b@x', city: 'london', name: 'anne', body: 'lock' };",
        )
        .expect("a schema");
    session
}

/// The `WHERE` of a read, parsed the way a statement parses it.
///
/// Not `parse_expression`: in a value position a bare name is a **table**
/// reference, and only the condition parser reads one as a route into the
/// record. Building the condition any other way would test a shape the
/// language never produces.
fn condition_of(written: &str) -> Expr {
    let script = parse(&format!("SELECT * FROM users WHERE {written};")).expect("a statement");
    let Some(StatementKind::Select(select)) =
        script.statements.first().map(|held| held.kind.clone())
    else {
        panic!("not a read");
    };
    match select.from {
        Source::Where { condition, .. } => *condition,
        other => panic!("not a filtered read: {other:?}"),
    }
}

/// Which index the planner picks for this condition, and on what ceiling.
fn planned(session: &Session<'_>, store: &Store, written: &str) -> (String, Rows) {
    let condition = condition_of(written);
    let mut transaction = store.begin().expect("a transaction");
    let table = Catalog::new(&mut transaction)
        .table_id(
            tessari_types::NamespaceId::new(1),
            tessari_types::DatabaseId::new(1),
            "users",
        )
        .expect("a lookup")
        .expect("the table");
    let declared = Catalog::new(&mut transaction)
        .indexes_on(table)
        .expect("the indexes");
    let searched = session
        .searched_for(&mut transaction, table, &[&condition])
        .expect("the searched context");
    let offered = session
        .enumerate(&mut transaction, &condition, &declared, &searched)
        .expect("the candidates");
    let chosen = choose(offered).expect("a candidate");
    (chosen.index.name, chosen.rows)
}

#[test]
fn a_unique_equality_is_chosen_over_one_written_before_it() {
    let store = store();
    let session = ready(&store);
    assert_eq!(
        planned(&session, &store, "city = 'london' AND email = 'a@x'"),
        ("by_email".to_owned(), Rows::AtMost(1))
    );
    // And the same the other way round, which is the point: the plan is not
    // a function of where the author put the clause.
    assert_eq!(
        planned(&session, &store, "email = 'a@x' AND city = 'london'"),
        ("by_email".to_owned(), Rows::AtMost(1))
    );
}

#[test]
fn an_equality_is_chosen_over_a_prefix_range_written_before_it() {
    let store = store();
    let session = ready(&store);
    for written in [
        "name LIKE 'a%' AND city = 'london'",
        "city = 'london' AND name LIKE 'a%'",
    ] {
        assert_eq!(
            planned(&session, &store, written).0,
            "by_city",
            "for {written}"
        );
    }
}

#[test]
fn a_term_carries_a_real_ceiling_and_wins_when_it_is_small() {
    // `df` is a cheap exact count, so a search candidate is the one kind of
    // unknown-shaped test that arrives with a number.
    let store = store();
    let session = ready(&store);
    let (name, rows) = planned(&session, &store, "city = 'london' AND body MATCHES 'lock'");
    assert_eq!(name, "by_body");
    assert_eq!(rows, Rows::AtMost(2));

    // A term nothing holds beats everything, because the read is empty.
    let (name, rows) = planned(
        &session,
        &store,
        "email = 'a@x' AND body MATCHES 'unheardof'",
    );
    assert_eq!(name, "by_body");
    assert_eq!(rows, Rows::AtMost(0));
}

#[test]
fn a_condition_no_index_can_serve_offers_nothing() {
    let store = store();
    let session = ready(&store);
    let condition = condition_of("nickname = 'ada'");
    let mut transaction = store.begin().expect("a transaction");
    let declared = Vec::new();
    let offered = session
        .enumerate(
            &mut transaction,
            &condition,
            &declared,
            &Searched::default(),
        )
        .expect("the candidates");
    assert!(offered.is_empty());
    assert!(choose(offered).is_none());
}
