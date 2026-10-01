//! G051 C5 — an ordered `LIMIT` over a split table ranks on the leaders
//! (ADR-0102): each shard sends its own first `n`, and the asker orders the
//! union, so the answer is the whole node's and only `n` per shard travels.
//!
//! Every read is compared with the same statement run on the leader, which
//! holds every shard — the unsplit mirror of ADR-0097 D4. The ties are the
//! cases that matter: the order is total only because ties fall to the record
//! id, and a shard sending its first `n` by value alone could leave out a
//! record the whole node keeps.

#![allow(clippy::panic, clippy::unwrap_used)]

use crate::gathered_reads::{Pair, answer, pair};

/// Records tying with ones already there, in the two shards the follower
/// lacks (below 'g' and from 'p'), so the follower's own copy is unchanged.
fn with_ties() -> Pair {
    let pair = pair();
    pair.on_the_leader("root")
        .run(
            "CREATE ledger:'d' = { total: 5, note: 'd' }; CREATE ledger:'e' = { total: 5, note: 'e' };\n\
             CREATE ledger:'r' = { total: 5, note: 'r' }; CREATE ledger:'s' = { total: 7, note: 's' };\n\
             CREATE ledger:'t' = { total: 2, note: 't' };",
        )
        .unwrap();
    pair
}

/// The identities an answer holds, in its order.
fn ids(records: &[(tessari_types::RecordId, tessari_types::Value)]) -> Vec<String> {
    records.iter().map(|(id, _)| id.to_string()).collect()
}

/// The records the last outcome of `read` answered.
fn records(
    outcomes: Vec<tessari_session::Outcome>,
    read: &str,
) -> Vec<(tessari_types::RecordId, tessari_types::Value)> {
    match outcomes.into_iter().last() {
        Some(tessari_session::Outcome::Records { records, .. }) => records,
        other => panic!("{read}: {other:?}"),
    }
}

/// Each read, with the `n` it keeps.
const ORDERED: [(&str, usize); 6] = [
    ("SELECT * FROM ledger ORDER BY total LIMIT 3;", 3),
    ("SELECT * FROM ledger ORDER BY total DESC LIMIT 4;", 4),
    // The cut falls inside the four records totalling 5.
    ("SELECT * FROM ledger ORDER BY total LIMIT 6;", 6),
    (
        "SELECT note, total FROM ledger ORDER BY total DESC LIMIT 3;",
        3,
    ),
    ("SELECT * FROM ledger ORDER BY total START 3 LIMIT 2;", 5),
    (
        "SELECT * FROM ledger WHERE total > 1 ORDER BY total DESC, note LIMIT 3;",
        3,
    ),
];

#[test]
fn an_ordered_limit_answers_what_the_whole_node_answers_and_sends_n_per_shard() {
    let pair = with_ties();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for (read, n) in ORDERED {
        pair.sent();
        let (gathered, _) = answer(&mut follower, read);
        // Two shards are gathered, so at most `n` from each.
        let sent = pair.sent();
        assert_eq!(gathered, answer(&mut whole, read).0, "{read}");
        assert!(sent <= n * 2, "{read}: {sent} records travelled for {n}");
    }
    // One answer pinned by hand, so the equality above is not two copies of one
    // mistake: 1 (b), 2 (h), 2 (t), 3 (q), then the 5s by identity.
    let (first, _) = answer(
        &mut follower,
        "SELECT * FROM ledger ORDER BY total LIMIT 6;",
    );
    assert_eq!(ids(&first), ["b", "h", "t", "q", "a", "d"]);
}

#[test]
fn an_exact_nearest_neighbour_read_ranks_on_the_leaders() {
    // `points` is split like `ledger` and this node holds none of it, so all
    // three shards are gathered; `q` and `r` tie at the same distance. The query
    // vector is written as the docs write it, and bound as a client binds it.
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let mut near = tessari_ql::Parameters::new();
    near.insert(
        "near".to_owned(),
        tessari_types::Value::Array(vec![
            tessari_types::Value::from(3),
            tessari_types::Value::from(3),
        ]),
    );
    for (read, parameters) in [
        (
            "SELECT * FROM points ORDER BY vector::euclidean(at, [3, 3]) LIMIT 2;",
            tessari_ql::Parameters::new(),
        ),
        (
            "SELECT * FROM points ORDER BY vector::euclidean(at, $near) LIMIT 2;",
            near,
        ),
    ] {
        pair.sent();
        let gathered = records(follower.run_with(read, &parameters).unwrap(), read);
        let sent = pair.sent();
        assert_eq!(
            gathered,
            records(whole.run_with(read, &parameters).unwrap(), read),
            "{read}"
        );
        assert!(sent <= 2 * 3, "{read}: {sent} records travelled for 2");
        assert_eq!(ids(&gathered), ["q", "r"], "{read}");
    }
}

#[test]
fn keys_that_cannot_travel_still_answer_the_whole_table() {
    // A key naming an alias reads the projection, which only this node makes;
    // a `WHERE` that cannot travel leaves the leader unable to know what this
    // node keeps. Records travel for both, and the answer is still the whole.
    let pair = with_ties();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in [
        "SELECT note AS total, total AS note FROM ledger ORDER BY total LIMIT 2;",
        "SELECT * FROM ledger WHERE total > 0 AND time::now() > datetime '2000-01-01T00:00:00Z' ORDER BY total LIMIT 2;",
    ] {
        pair.sent();
        let (gathered, _) = answer(&mut follower, read);
        assert!(pair.sent() > 4, "{read}: the keys travelled");
        assert_eq!(gathered, answer(&mut whole, read).0, "{read}");
    }
}

/// G051 C5, end to end: a shard holding more records than a gather may hold,
/// ordered and bounded on the leader.
#[test]
#[ignore = "inserts 100 001 records — about three minutes in a debug build; \
            G051 C5's own validation, run explicitly: cargo test -p tessari-session \
            --test suite an_ordered_limit_over_a_shard_past_the_ceiling -- --ignored"]
fn an_ordered_limit_over_a_shard_past_the_ceiling_answers() {
    let pair = pair();
    let past = tessari_constants::GATHER_RECORDS + 1;
    // Generated identities sort after every text one, so all of these land in
    // shard 3, which this node lacks.
    let mut insert = String::from("INSERT INTO ledger (total, note) VALUES ");
    for n in 0..past {
        if n > 0 {
            insert.push_str(", ");
        }
        insert.push_str(&format!("({}, 'many')", n % 1000));
    }
    insert.push(';');
    pair.on_the_leader("root").run(&insert).unwrap();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let read = "SELECT * FROM ledger ORDER BY total DESC LIMIT 10;";
    let (gathered, _) = answer(&mut follower, read);
    assert_eq!(gathered, answer(&mut whole, read).0);
    assert_eq!(gathered.len(), 10);
}
