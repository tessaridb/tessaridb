//! A follower behind its leader's pruned log, brought to the leader's state by a
//! copy (ADR-0094 D3, amendment 4').
//!
//! The copy is the leader's state applied over what the follower holds, then
//! every record the copy did not rewrite removed. These tests hold the claim
//! that matters: afterwards the follower answers exactly what the leader
//! answers, records the leader deleted are gone, and every derived entry is the
//! leader's own — not merely that the copy ran.

use tessari_kv::Keyspace;
use tessari_storage::Store;
use tessari_types::Reach;

use crate::restore::{INTERROGATION, dump, original, signed_in, store};

/// Copy `source`'s state at `within` onto `onto`, the way a follower re-seeds.
fn copied(source: &Store, onto: &Store, within: Reach) -> u64 {
    let before = onto.committed_version().unwrap();
    let mut reader = source.read_state_within(within).unwrap();
    let positions = reader.positions().to_vec();
    while let Some(chunk) = reader.next_chunk(500).unwrap() {
        onto.restore_state_chunk(&chunk).unwrap();
    }
    let topics = reader.topic_heads().unwrap();
    drop(reader);
    let removed = onto.sweep_unreplaced(within, before).unwrap();
    onto.finish_state(&positions, &topics).unwrap();
    removed
}

/// The leader's history after the follower last heard from it.
const MOVED_ON: &str = "\
USE NAMESPACE prod; USE DATABASE orders;\n\
DELETE people:3;\n\
UPDATE people:1 MERGE { bio: 'a compiler for locks', address: { city: 'paris' } };\n\
CREATE people:5 = { name: 'barbara', email: 'e@x', bio: 'lock free', address: { city: 'paris' }, at: [3.0, 0.0] };\n\
DEL sessions:'abc';\n\
DELETE media:'/notes.txt';\n\
DEFINE TABLE gone SCHEMALESS; CREATE gone:1 = { x: 1 }; DROP TABLE gone;\n\
DEFINE TABLE kept SCHEMALESS; CREATE kept:1 = { x: 2 };";

#[test]
fn a_follower_that_fell_behind_answers_what_its_leader_answers_after_a_copy() {
    let (_, source, _) = original();
    let (follower_backend, follower) = store();
    copied(&source, &follower, Reach::Store);
    signed_in(&source).run(MOVED_ON).unwrap();

    let removed = copied(&source, &follower, Reach::Store);
    assert!(
        removed >= 3,
        "people:3, sessions:'abc' and media:'/notes.txt' are gone at the leader, \
         and the sweep removed {removed}"
    );

    let mut here = signed_in(&source);
    let mut there = signed_in(&follower);
    for script in INTERROGATION
        .iter()
        .copied()
        .chain(["SELECT * FROM kept;", "INFO FOR DATABASE;"])
    {
        assert_eq!(
            format!("{:?}", here.run(script).unwrap()),
            format!("{:?}", there.run(script).unwrap()),
            "the follower and its leader disagree about {script}"
        );
    }

    // The derived entries are the leader's own: an index that kept an entry for
    // a removed record, or missed one for a changed record, is a difference in
    // this keyspace even where no read above happened to ask. Two kinds are
    // compared by key only or not at all, and both for a stated reason: a
    // vector node's neighbour list depends on the order its graph was built in
    // (ADR-0091 amendment 4), and recall and refinement are measurements, which
    // no copy carries.
    let comparable = |entries: Vec<(Vec<u8>, Vec<u8>)>| -> Vec<(Vec<u8>, Vec<u8>)> {
        entries
            .into_iter()
            .filter(|(key, _)| !matches!(key.first(), Some(&(VECTOR_RECALL | SPATIAL_REFINEMENT))))
            .map(|(key, value)| match key.first() {
                Some(&VECTOR_NODE) => (key, Vec::new()),
                _ => (key, value),
            })
            .collect()
    };
    assert_eq!(
        comparable(rebuilt_index(&source)),
        comparable(dump(&follower_backend, Keyspace::INDEX)),
        "the follower's index keyspace is not the leader's rebuilt one"
    );
}

/// Key-kind tags (docs/key-grammar.md §3).
const VECTOR_NODE: u8 = 0x13;
const VECTOR_RECALL: u8 = 0x17;
const SPATIAL_REFINEMENT: u8 = 0x18;

/// The leader's index keyspace as a fresh copy derives it — what a follower that
/// received the state must hold, byte for byte.
fn rebuilt_index(source: &Store) -> Vec<(Vec<u8>, Vec<u8>)> {
    let (backend, fresh) = store();
    copied(source, &fresh, Reach::Store);
    dump(&backend, Keyspace::INDEX)
}
