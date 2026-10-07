//! A node holding part of a split expiring table gathers the rest from the
//! shards' leaders, and a record past its instant is answered by none of them
//! (ADR-0122 A7; G069 C2, Q-953).
//!
//! The expiring records sit only in the shards this node does not hold, so the
//! gather is the one path that could answer them. Both the gathered answer and
//! the whole node's go through the same filter, so each read is held to the
//! ids it must answer rather than to the other answer alone.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;
use std::time::Duration as Wait;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

use crate::gathered_reads::{Pair, answer, follower_of_the_middle_of, pair_of, signed_in};

const PAST_SHORT: Wait = Wait::from_millis(450);

/// A leader holding `message` split at 'g' and 'p', expiring after 300 ms:
/// the expiring records in the first and last shards, a permanent one in
/// every shard.
fn leader() -> Arc<Store> {
    let leader = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&leader)
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE message (n int) IDENTITY uuid SPLIT AT 'g', 'p' EXPIRE AFTER 300ms;\n\
             CREATE message:'a' = { n: 1 }; CREATE message:'z' = { n: 1 };\n\
             CREATE message:'b' = { n: 1 } EXPIRE NONE; CREATE message:'k' = { n: 1 } EXPIRE NONE;\n\
             CREATE message:'y' = { n: 1 } EXPIRE NONE;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    signed_in(&leader, "root")
        .run(
            "DEFINE USER reader ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';\n\
             DEFINE USER node AUTHORITIES replicate PASSWORD 'correct horse battery';",
        )
        .unwrap();
    Arc::new(leader)
}

fn pair() -> Pair {
    let leader = leader();
    let follower = follower_of_the_middle_of(&leader, "message");
    pair_of(leader, follower)
}

/// The ids a read answers.
fn ids(session: &mut Session<'_>, read: &str) -> BTreeSet<String> {
    answer(session, read)
        .0
        .into_iter()
        .map(|(id, _)| format!("{id}"))
        .collect()
}

fn named(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|key| (*key).to_owned()).collect()
}

const READS: [&str; 3] = [
    "SELECT * FROM message;",
    "SELECT * FROM message WHERE n = 1;",
    "SELECT * FROM message:'a'..='z';",
];

#[test]
fn a_gathered_read_answers_no_record_past_its_instant_from_a_shard_it_does_not_hold() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let every = named(&["a", "b", "k", "y", "z"]);
    let kept = named(&["b", "k", "y"]);
    for read in READS {
        assert_eq!(
            ids(&mut follower, read),
            every,
            "{read}: before the instant"
        );
    }
    thread::sleep(PAST_SHORT);
    for read in READS {
        let gathered = ids(&mut follower, read);
        assert_eq!(gathered, kept, "{read}: after the instant");
        assert_eq!(gathered, ids(&mut whole, read), "{read}: the whole node");
    }
    assert!(
        !pair.asked().is_empty(),
        "the follower answered without gathering"
    );
}
