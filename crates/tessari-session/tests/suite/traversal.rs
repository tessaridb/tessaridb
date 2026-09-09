//! Walks longer than one hop.
//!
//! One hop is covered by the corpus, and what it covers is that traversal *is*
//! an index read. What a chain adds is arithmetic on sets, and the three things
//! that can go wrong with it are not visible in a single hop:
//!
//! - **duplication**, because two paths can reach one record and this store's
//!   answers are keyed by record;
//! - **a grant hole**, because a chain passes through tables that were never
//!   named in the grant somebody actually holds;
//! - **cycles**, which are data rather than a mistake and must not be quietly
//!   filtered out.
//!
//! Each expected set below is derived by hand from the fixture rather than by
//! asking the store the one-hop question twice — a test that computes its
//! expectation with the code it is testing proves that the code agrees with
//! itself.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A small social graph, written out so the answers are readable by inspection.
///
/// ```text
/// 1 -> 2 -> 4
/// 1 -> 3 -> 4        4 is reached twice: the diamond
/// 1 -> 2 -> 1        and back to the start: the cycle
/// 4 -> 5             one hop further, for the three-hop case
/// ```
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE social; USE DATABASE social;\n\
             DEFINE COLLECTION users;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             CREATE users:3 = { handle: 'katherine' };\n\
             CREATE users:4 = { handle: 'dorothy' };\n\
             CREATE users:5 = { handle: 'margaret' };\n\
             RELATE users:1->follows->users:2;\n\
             RELATE users:1->follows->users:3;\n\
             RELATE users:2->follows->users:4;\n\
             RELATE users:3->follows->users:4;\n\
             RELATE users:2->follows->users:1;\n\
             RELATE users:4->follows->users:5;",
        )
        .unwrap();
    session
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

fn users(names: &[i64]) -> Vec<RecordId> {
    let mut held: Vec<RecordId> = names.iter().map(|n| RecordId::Int(*n)).collect();
    held.sort();
    held
}

#[test]
fn two_hops_reach_what_one_hop_reaches_from_each_of_the_first_hops_answers() {
    // ada follows grace and katherine; both follow dorothy, and grace also
    // follows ada back. So the two-hop answer is dorothy and ada.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM users:1->follows->users->follows->users;"
        ),
        users(&[1, 4])
    );
}

#[test]
fn a_record_two_paths_reach_is_answered_once() {
    // The diamond: dorothy is reached through grace and through katherine. An
    // answer is keyed by record, so twice would be a wrong answer rather than a
    // repetitive one — and a caller counting rows would count two followers of
    // followers where there is one person.
    let store = store();
    let mut session = ready(&store);
    let found = ids(
        &mut session,
        "SELECT * FROM users:1->follows->users->follows->users;",
    );
    assert_eq!(
        found.iter().filter(|id| **id == RecordId::Int(4)).count(),
        1,
        "{found:?}"
    );
}

#[test]
fn a_walk_that_returns_to_where_it_started_says_so() {
    // grace follows ada back, so ada is genuinely two hops from ada. Filtering
    // her out would be the store deciding the question was not meant.
    let store = store();
    let mut session = ready(&store);
    let found = ids(
        &mut session,
        "SELECT * FROM users:1->follows->users->follows->users;",
    );
    assert!(found.contains(&RecordId::Int(1)), "{found:?}");
}

#[test]
fn three_hops_are_not_a_special_case_of_two() {
    // From ada: {grace, katherine} → {dorothy, ada} → {margaret} ∪ {grace,
    // katherine}. Margaret is only reachable in three, which is what makes this
    // a test of the loop rather than of a second hard-coded step.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM users:1->follows->users->follows->users->follows->users;"
        ),
        users(&[2, 3, 5])
    );
}

#[test]
fn a_chain_may_end_on_the_edges_rather_than_their_far_side() {
    // `…->users->follows` stops at the edge records of the last step, the way a
    // single `users:1->follows` does. Two of ada's follows have edges out —
    // grace has two, katherine has one — so three edges.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT * FROM users:1->follows->users->follows;")
        .unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 3);
}

#[test]
fn a_dangling_endpoint_mid_chain_drops_its_path_and_fails_nothing() {
    // katherine is deleted while her edges remain. The path through her stops;
    // the path through grace does not, so dorothy still comes back — reached the
    // other way round the diamond.
    let store = store();
    let mut session = ready(&store);
    session.run("DELETE users:3;").unwrap();
    let found = ids(
        &mut session,
        "SELECT * FROM users:1->follows->users->follows->users;",
    );
    assert_eq!(found, users(&[1, 4]));
}

#[test]
fn every_table_in_the_chain_is_asked_about_and_not_only_the_first() {
    // The hole this could have opened. A caller granted on `users` and `follows`
    // walks two hops through both; a caller granted on the first table alone
    // must not arrive anywhere, because a chain passes through the same tables
    // repeatedly and a grant asked once is a grant asked for the wrong thing.
    //
    // Modelled with a *second* edge table so the chain names a table the grant
    // never covers: ada -> follows -> users -> wrote -> posts.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION posts;\n\
             DEFINE TABLE wrote EDGE;\n\
             CREATE posts:1 = { title: 'notes on the engine' };\n\
             RELATE users:2->wrote->posts:1;\n\
             DEFINE USER root ROLE owner PASSWORD 'a long one';",
        )
        .unwrap();
    // The first user closed the store, so everything after it is declared by
    // the owner — the shape every store past its first day is in.
    let mut root = Session::new(&store);
    root.sign_in("root", "a long one").unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE social;\n\
         DEFINE USER ada ON prod.social ROLE editor PASSWORD 'a long one';\n\
         GRANT read ON users TO ada;\n\
         GRANT read ON follows TO ada;",
    )
    .unwrap();

    let mut narrow = Session::new(&store);
    narrow.sign_in("ada", "a long one").unwrap();
    narrow
        .run("USE NAMESPACE prod; USE DATABASE social;")
        .unwrap();

    // One hop is granted, so it answers.
    assert!(narrow.run("SELECT * FROM users:1->follows->users;").is_ok());

    // The second hop leaves the granted tables, and is refused rather than
    // answered with the posts.
    let refused = narrow.run("SELECT * FROM users:1->follows->users->wrote->posts;");
    assert!(
        matches!(refused, Err(Error::NotGranted { .. })),
        "a chain reached an ungranted table: {refused:?}"
    );
}

#[test]
fn the_arrows_of_a_chain_all_point_the_same_way() {
    // Within a step a mixed pair is a query nobody means; across steps it asks a
    // real question — "who follows somebody ada follows" — and is a design of its
    // own rather than a rule quietly relaxed here.
    let store = store();
    let mut session = ready(&store);
    let refused = session.run("SELECT * FROM users:1->follows->users<-follows<-users;");
    assert!(
        matches!(refused, Err(Error::Script(_))),
        "a mixed chain parsed: {refused:?}"
    );
}

#[test]
fn a_chain_walked_backwards_asks_the_mirrored_question() {
    // Who follows somebody who follows dorothy? Dorothy is followed by grace and
    // katherine; both of them are followed by ada and by nobody else. So the
    // answer is ada — once, although two paths reach her, which is the diamond
    // again seen from the other end.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        ids(
            &mut session,
            "SELECT * FROM users:4<-follows<-users<-follows<-users;"
        ),
        users(&[1])
    );
}

/// An edge created inside this transaction is followed by this transaction.
///
/// The traversal is an index read, and W188 found the vector walk answering from
/// a graph that could not contain the record the reading transaction had just
/// written. Entries are derived at commit, so the question is the same one here
/// — and the answer must be, too: a `RELATE` and the walk that follows it are
/// routinely written in one script, and a traversal that could not see its own
/// edge would answer a short set with nothing in an error state.
///
/// Unlike the vector walk there is **no scan to fall back to** — an edge table's
/// direction indexes are how edges are followed, not an optimisation over
/// following them — so if this ever regresses the fix is not a refusal. That is
/// why the case is worth pinning rather than assuming.
#[test]
fn an_edge_written_in_this_transaction_is_followed_by_it() {
    let store = store();
    let mut session = ready(&store);

    let outcomes = session
        .run(
            "BEGIN;\n\
             RELATE users:5->follows->users:3;\n\
             SELECT * FROM users:5->follows->users;\n\
             COMMIT;",
        )
        .unwrap();
    // 0 is `BEGIN`, 1 is the `RELATE`, 2 is the read.
    let mut found: Vec<RecordId> = outcomes[2]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();

    assert_eq!(
        found,
        users(&[3]),
        "the edge this transaction wrote was not followed by it: {found:?}"
    );
}
