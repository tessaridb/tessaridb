//! G051 C6 — full-text and geometric reads over a split table answer on a
//! partial holder what the whole node answers.
//!
//! `docs` is split at 'g' and 'p'; the follower holds the middle shard and
//! gathers the other two. Every read is compared with the same statement on
//! the leader, which holds every shard — the unsplit mirror of ADR-0097 D4.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use crate::gathered_reads::{Pair, answer, follower_of_the_middle_of, leader, pair_of};

/// A word that is rare in the middle shard and common in the others, so a
/// score measured against the middle alone weighs it differently.
fn searched() -> Pair {
    let leader = leader();
    let mut root = crate::gathered_reads::signed_in(&leader, "root");
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;\n\
         DEFINE TABLE docs (body string ANALYZER english, at geometry) IDENTITY uuid SPLIT AT 'g', 'p';\n\
         DEFINE INDEX by_body ON docs FIELDS body SEARCH;\n\
         CREATE docs:'a' = { body: 'the quick fox jumps', at: geometry { type: 'Point', coordinates: [1, 1] } };\n\
         CREATE docs:'b' = { body: 'a fox and a fox', at: geometry { type: 'Point', coordinates: [9, 9] } };\n\
         CREATE docs:'c' = { body: 'foxes running', at: geometry { type: 'Point', coordinates: [2, 2] } };\n\
         CREATE docs:'h' = { body: 'the lazy dog sleeps', at: geometry { type: 'Point', coordinates: [3, 3] } };\n\
         CREATE docs:'k' = { body: 'a fox among dogs and dogs', at: geometry { type: 'Point', coordinates: [8, 8] } };\n\
         CREATE docs:'m' = { body: 'dogs dogs dogs', at: geometry { type: 'Point', coordinates: [4, 4] } };\n\
         CREATE docs:'q' = { body: 'quick brown fox', at: geometry { type: 'Point', coordinates: [5, 5] } };\n\
         CREATE docs:'z' = { body: 'the end of the fox story', at: geometry { type: 'Point', coordinates: [7, 7] } };",
    )
    .unwrap();
    let follower = follower_of_the_middle_of(&leader, "docs");
    pair_of(Arc::clone(&leader), follower)
}

/// The records, and their values, the last outcome of `read` answered.
fn records(
    session: &mut tessari_session::Session<'_>,
    read: &str,
) -> Vec<(String, tessari_types::Value)> {
    answer(session, read)
        .0
        .into_iter()
        .map(|(id, value)| (id.to_string(), value))
        .collect()
}

/// The suggestion the last outcome of `read` carried.
fn suggestion(
    session: &mut tessari_session::Session<'_>,
    read: &str,
) -> Option<tessari_session::Suggestion> {
    match session.run(read).unwrap().into_iter().last() {
        Some(tessari_session::Outcome::Records { suggestion, .. }) => suggestion,
        other => panic!("{read}: {other:?}"),
    }
}

const HERE: &str = "geometry { type: 'Point', coordinates: [0, 0] }";

#[test]
fn a_score_over_a_gathered_read_is_measured_against_the_whole_collection() {
    let pair = searched();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in [
        "SELECT id, search::score(body, 'fox') AS s FROM docs WHERE body MATCHES 'fox' ORDER BY s DESC;",
        "SELECT id, search::score(body, 'fox dog') AS s FROM docs ORDER BY s DESC, id;",
        "SELECT id FROM docs ORDER BY search::score(body, 'dogs') DESC LIMIT 2;",
        &format!(
            "SELECT id FROM docs ORDER BY FUSE (search::score(body, 'fox') DESC, geo::distance(at, {HERE})) LIMIT 3;"
        ),
    ] {
        assert_eq!(
            records(&mut follower, read),
            records(&mut whole, read),
            "{read}"
        );
    }
    // Pinned by hand, so the equality is not two copies of one mistake: `b`
    // says fox twice in five words, `k` once in six.
    let read =
        "SELECT id FROM docs WHERE body MATCHES 'fox' ORDER BY search::score(body, 'fox') DESC;";
    let ids: Vec<String> = records(&mut follower, read)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, ["b", "c", "q", "a", "k", "z"]);
}

#[test]
fn matching_highlighting_and_shapes_over_a_gathered_read_are_the_whole_nodes() {
    let pair = searched();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in [
        "SELECT id FROM docs WHERE body MATCHES 'fox';",
        "SELECT id FROM docs WHERE body MATCHES PREFIX 'qui';",
        "SELECT id FROM docs WHERE body MATCHES 'dog' AND body MATCHES 'lazy';",
        "SELECT id, search::highlight(body) AS h FROM docs WHERE body MATCHES 'fox';",
        "SELECT id FROM docs WHERE geo::within(at, geometry { type: 'Polygon', coordinates: [[[0, 0], [5, 0], [5, 5], [0, 5], [0, 0]]] });",
        &format!("SELECT id FROM docs ORDER BY geo::distance(at, {HERE}) LIMIT 3;"),
    ] {
        assert_eq!(
            records(&mut follower, read),
            records(&mut whole, read),
            "{read}"
        );
    }
    let ids: Vec<String> = records(
        &mut follower,
        "SELECT id FROM docs WHERE body MATCHES PREFIX 'qui';",
    )
    .into_iter()
    .map(|(id, _)| id)
    .collect();
    assert_eq!(ids, ["a", "q"]);
}

#[test]
fn a_partial_holder_offers_no_suggestion_its_own_dictionary_cannot_stand_behind() {
    // `quick` is only in the shards the follower lacks, so its dictionary
    // alone would say nothing is nearer than `quik`.
    let pair = searched();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let read = "SELECT id FROM docs WHERE body MATCHES 'quik';";
    assert!(
        suggestion(&mut whole, read).is_some(),
        "the control: the whole node suggests"
    );
    assert_eq!(suggestion(&mut follower, read), None);
}

/// The leader's half of ADR-0103, against the index it stands in for: what
/// `search_counts` makes of every record of the table is what the index's own
/// statistics and dictionary say about the same records.
#[test]
fn counting_every_record_agrees_with_the_index_it_stands_in_for() {
    let pair = searched();
    let leader = pair.leader();
    let mut transaction = leader.begin().unwrap();
    let table = tessari_storage::Catalog::new(&mut transaction)
        .table_id(
            tessari_types::NamespaceId::new(1),
            tessari_types::DatabaseId::new(1),
            "docs",
        )
        .unwrap()
        .unwrap();
    let index = tessari_storage::Catalog::new(&mut transaction)
        .indexes_on(table)
        .unwrap()
        .into_iter()
        .find(|index| index.search)
        .unwrap();
    let every = transaction
        .records_between(
            tessari_types::NamespaceId::new(1),
            tessari_types::DatabaseId::new(1),
            table,
            tessari_storage::Window::default(),
            None,
            usize::MAX,
        )
        .unwrap();
    let terms = ["fox", "dog", "quick", "absent"].map(str::to_owned);
    let counted = transaction.search_counts(&index, &every, &terms).unwrap();
    let statistics = transaction.search_statistics(&index).unwrap();
    assert_eq!(
        (counted.documents, counted.tokens),
        (statistics.documents, statistics.terms)
    );
    let held: Vec<u64> = terms
        .iter()
        .map(|term| transaction.term_statistics(&index, term).unwrap().documents)
        .collect();
    assert_eq!(counted.holding, held);
    // Pinned, so the equality is not two copies of one mistake: eight
    // documents; `fox` stemmed in six of them, `dog` in three.
    assert_eq!(counted.documents, 8);
    assert_eq!(counted.holding, [6, 3, 2, 0]);
}

/// A starred word is scored against the terms the **collection** holds under it
/// (ADR-0104 D5), and a node holding part of the table holds part of the
/// dictionary — the shards it lacks hold words of their own beginning with
/// `fox`, which it cannot rank. So the score is refused with
/// the refusal a part-holder gives, and the same read on the whole node scores;
/// matching on the starred word still gathers, because whether a record holds
/// a word beginning with `fox` is a question about that record alone.
#[test]
fn a_starred_word_is_not_scored_on_a_part_of_the_dictionary() {
    let pair = searched();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let scored = "SELECT id, search::score(body, 'fox*') AS s FROM docs ORDER BY s DESC, id;";
    let refused = follower.run(scored);
    assert!(
        matches!(refused, Err(tessari_session::Error::NotHeldHere { ref table, .. }) if table == "docs"),
        "{refused:?}"
    );
    assert_eq!(records(&mut whole, scored).len(), 8);

    let matched = "SELECT id FROM docs WHERE body MATCHES 'fox*' ORDER BY id;";
    let found = records(&mut follower, matched);
    assert_eq!(found, records(&mut whole, matched));
    let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c", "k", "q", "z"]);
}
