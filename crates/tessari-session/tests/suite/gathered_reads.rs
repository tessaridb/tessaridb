//! G033 — a node holding part of a split table answers a read of the whole of
//! it, by gathering the shards it lacks from their leaders (ADR-0083).
//!
//! The gatherer here reads the leader's own store, which is what the peer door
//! does on the other end of a real link; what these tests are about is the
//! session's half — which parts it asks for, what it does with the answer, and
//! when it refuses instead. Every read is compared against the same statement
//! run on the leader, which holds every shard: a gathered answer is right when it
//! is the answer a whole node gives, and an expectation written by hand would
//! agree with whichever of the two it was written from.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Asked, Gather, Gathered, Note, Outcome, Session, Unanswered};
use tessari_storage::{Reach, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, ShardId, TableId, Value};

const PASSWORD: &str = "correct horse battery";
const A_FOLLOWER: [u8; 16] = [7; 16];
const THE_LEADER: [u8; 16] = [9; 16];

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// A leader holding `ledger` split at 'g' and 'p', with records in all three
/// shards, an owner, a reader, a reader who may see only `note`, and a node.
fn leader() -> Arc<Store> {
    let leader = store();
    Session::new(&leader)
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE ledger (total int, note string, peer record) IDENTITY uuid \
             SPLIT AT 'g', 'p';\n\
             DEFINE TABLE other (total int) IDENTITY uuid;\n\
             CREATE other:'x' = { total: 1 };\n\
             CREATE ledger:'a' = { total: 5, note: 'a' }; CREATE ledger:'b' = { total: 1, note: 'b' };\n\
             CREATE ledger:'c' = { total: 9, note: 'c' };\n\
             CREATE ledger:'h' = { total: 2, note: 'h', peer: ledger:'a' };\n\
             CREATE ledger:'k' = { total: 7, note: 'k' };\n\
             CREATE ledger:'q' = { total: 3, note: 'q' }; CREATE ledger:'z' = { total: 8, note: 'z' };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    signed_in(&leader, "root")
        .run(
            "DEFINE USER reader ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';\n\
             DEFINE USER narrow ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';\n\
             USE NAMESPACE prod; USE DATABASE shop; GRANT read ON ledger FIELDS note TO narrow;\n\
             DEFINE USER node AUTHORITIES replicate PASSWORD 'correct horse battery';",
        )
        .unwrap();
    Arc::new(leader)
}

fn ledger(store: &Store) -> TableId {
    let mut transaction = store.begin().unwrap();
    tessari_storage::Catalog::new(&mut transaction)
        .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
        .unwrap()
        .unwrap()
}

fn shard(store: &Store, id: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        ledger(store),
        ShardId::new(id),
    )
}

/// A follower of shard 2 only, recorded as served that way.
fn follower_of_the_middle(leader: &Store) -> Store {
    let over = shard(leader, 2);
    let follower = store();
    let mut node = signed_in(leader, "node");
    for log in leader.logs().unwrap() {
        let carried = node
            .replicate_from(leader, A_FOLLOWER, over, log, Sequence::new(1), 256)
            .unwrap();
        let mut previous = tessari_types::Epoch::ZERO;
        for (sequence, record) in carried {
            follower
                .apply_from_stream(log, sequence, previous, &record)
                .unwrap();
            previous = record.epoch();
        }
    }
    follower.record_served(over).unwrap();
    follower
}

/// One question the session put to the gatherer: the shard and the window.
type Question = (u32, Option<RecordId>, Option<(RecordId, bool)>);

/// The leader's store, answering as its peer door would, and remembering what
/// it was asked.
#[derive(Debug)]
struct FromTheLeader {
    leader: Arc<Store>,
    asked: Mutex<Vec<Question>>,
}

impl Gather for FromTheLeader {
    fn gather(&self, asked: &Asked<'_>) -> Result<Gathered, Unanswered> {
        self.asked.lock().unwrap().push((
            asked.shard.get(),
            asked.window.from.cloned(),
            asked
                .window
                .to
                .map(|(id, inclusive)| (id.clone(), inclusive)),
        ));
        let transaction = self.leader.begin().unwrap();
        let records = transaction
            .records_between(
                asked.namespace,
                asked.database,
                asked.table,
                asked.window,
                None,
                asked.most.saturating_add(1),
            )
            .unwrap();
        if records.len() > asked.most {
            return Err(Unanswered::Ceiling);
        }
        Ok(Gathered {
            records,
            node: THE_LEADER,
        })
    }
}

/// A gatherer whose leader cannot be reached.
#[derive(Debug)]
struct Unreachable;

impl Gather for Unreachable {
    fn gather(&self, _: &Asked<'_>) -> Result<Gathered, Unanswered> {
        Err(Unanswered::Refused("nobody answered".to_owned()))
    }
}

/// A gatherer that ignores the ceiling it was given and hands back more.
#[derive(Debug)]
struct Flooding;

impl Gather for Flooding {
    fn gather(&self, asked: &Asked<'_>) -> Result<Gathered, Unanswered> {
        let records = (0..=asked.most)
            .map(|n| (RecordId::from(format!("a{n:07}").as_str()), Vec::new()))
            .collect();
        Ok(Gathered {
            records,
            node: THE_LEADER,
        })
    }
}

struct Pair {
    leader: Arc<Store>,
    follower: Store,
    gatherer: Arc<FromTheLeader>,
}

fn pair() -> Pair {
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    let gatherer = Arc::new(FromTheLeader {
        leader: Arc::clone(&leader),
        asked: Mutex::new(Vec::new()),
    });
    Pair {
        leader,
        follower,
        gatherer,
    }
}

impl Pair {
    fn on_the_follower(&self, user: &str) -> Session<'_> {
        let mut session = signed_in(&self.follower, user)
            .gathering(Arc::clone(&self.gatherer) as Arc<dyn Gather>);
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        session
    }

    fn on_the_leader(&self, user: &str) -> Session<'_> {
        let mut session = signed_in(&self.leader, user);
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        session
    }

    fn asked(&self) -> Vec<Question> {
        std::mem::take(&mut *self.gatherer.asked.lock().unwrap())
    }
}

fn answer(session: &mut Session<'_>, read: &str) -> (Vec<(RecordId, Value)>, Vec<Note>) {
    match session.run(read) {
        Ok(outcomes) => match outcomes.last() {
            Some(Outcome::Records { records, notes, .. }) => (records.clone(), notes.clone()),
            other => panic!("{read}: {other:?}"),
        },
        Err(error) => panic!("{read}: {error:?}"),
    }
}

fn refused(session: &mut Session<'_>, read: &str) -> tessari_session::Error {
    match session.run(read) {
        Err(error) => error,
        Ok(outcomes) => panic!("{read}: expected a refusal, got {outcomes:?}"),
    }
}

/// S1.1 — every shape of read answers on the partial node what the whole node
/// answers, in the same order.
#[test]
fn a_gathered_read_answers_what_a_node_holding_every_shard_answers() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in [
        "SELECT * FROM ledger;",
        "SELECT count(*) AS n FROM ledger;",
        "SELECT * FROM ledger WHERE total > 2;",
        "SELECT note, total FROM ledger ORDER BY total DESC LIMIT 3;",
        "SELECT sum(total) AS sum, note FROM ledger GROUP BY note;",
        "SELECT * FROM ledger:'a';",
        "SELECT * FROM ledger:'b'..'q';",
        "SELECT * FROM ledger:'b'..='q';",
        "SELECT * FROM ledger:'h'..'k';",
    ] {
        let (gathered, _) = answer(&mut follower, read);
        let (expected, _) = answer(&mut whole, read);
        assert!(!expected.is_empty(), "{read}: the control answered nothing");
        assert_eq!(gathered, expected, "{read}");
    }
}

/// S1.2 — a read asks for exactly the shards and windows it needs.
#[test]
fn a_read_asks_only_for_the_shards_and_the_window_it_needs() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let id = |text: &str| RecordId::from(text);

    answer(&mut follower, "SELECT * FROM ledger:'a';");
    assert_eq!(
        pair.asked(),
        vec![(1, Some(id("a")), Some((id("a"), true)))]
    );

    answer(&mut follower, "SELECT * FROM ledger:'b'..'q';");
    assert_eq!(
        pair.asked(),
        vec![
            (1, Some(id("b")), Some((id("g"), false))),
            (3, Some(id("p")), Some((id("q"), false))),
        ]
    );

    answer(&mut follower, "SELECT * FROM ledger;");
    assert_eq!(
        pair.asked(),
        vec![(1, None, Some((id("g"), false))), (3, Some(id("p")), None),]
    );

    // Inside what it holds, it asks nothing.
    answer(&mut follower, "SELECT * FROM ledger:'h'..'k';");
    answer(&mut follower, "SELECT * FROM ledger:'k';");
    assert_eq!(pair.asked(), Vec::<Question>::new());
}

/// S1.3 — a gathered answer says so, and one that gathered nothing does not.
#[test]
fn a_gathered_answer_carries_a_note_naming_the_shards() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let (_, notes) = answer(&mut follower, "SELECT * FROM ledger;");
    assert_eq!(
        notes,
        vec![Note::Gathered {
            table: "ledger".to_owned(),
            shards: vec![1, 3],
        }]
    );
    let gathered = notes.first().unwrap();
    assert_eq!(gathered.kind(), "gathered");
    assert!(
        gathered.message().contains("not one snapshot"),
        "{}",
        gathered.message()
    );
    let (_, notes) = answer(&mut follower, "SELECT * FROM ledger:'h'..'k';");
    assert!(notes.is_empty(), "{notes:?}");
}

/// S1.4 — where a snapshot is the promise, or where this goal does not gather,
/// the partial node still refuses; and a gather that cannot complete refuses
/// the whole read.
#[test]
fn a_read_that_cannot_be_gathered_whole_is_refused_and_never_answered_in_part() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let not_held = |error: tessari_session::Error| match error {
        tessari_session::Error::NotHeldHere { table, shards } => (table, shards),
        other => panic!("expected NotHeldHere, got {other:?}"),
    };
    let held = ("ledger".to_owned(), vec![1, 3]);
    assert_eq!(
        not_held(refused(
            &mut follower,
            "BEGIN; SELECT * FROM ledger; COMMIT;"
        )),
        held
    );
    follower.run("CANCEL;").ok();
    assert_eq!(
        not_held(refused(&mut follower, "SELECT * FROM ledger VERSION 5;")),
        held
    );
    assert_eq!(
        not_held(refused(
            &mut follower,
            "SELECT * FROM ledger:'h' FETCH peer;"
        )),
        ("ledger".to_owned(), vec![1])
    );
    assert_eq!(
        not_held(refused(
            &mut follower,
            "SELECT * FROM other JOIN ledger ON other.total = ledger.total;"
        ))
        .0,
        "ledger"
    );
    assert_eq!(pair.asked(), Vec::<Question>::new(), "none of these asked");

    let mut stranded = signed_in(&pair.follower, "reader").gathering(Arc::new(Unreachable));
    stranded
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    match refused(&mut stranded, "SELECT * FROM ledger;") {
        tessari_session::Error::NotGathered { table, shard, why } => {
            assert_eq!((table.as_str(), shard), ("ledger", 1));
            assert!(why.contains("nobody answered"), "{why}");
        }
        other => panic!("expected NotGathered, got {other:?}"),
    }

    let mut flooded = signed_in(&pair.follower, "reader").gathering(Arc::new(Flooding));
    flooded
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    match refused(&mut flooded, "SELECT * FROM ledger;") {
        tessari_session::Error::GatheredTooMuch { table, most } => {
            assert_eq!(table, "ledger");
            assert_eq!(most, tessari_constants::GATHER_RECORDS);
        }
        other => panic!("expected GatheredTooMuch, got {other:?}"),
    }
}

/// S1.5 — a field this session may not read is as absent from a gathered
/// record as from a local one, to the projection and to the condition.
#[test]
fn a_hidden_field_is_hidden_in_gathered_records_too() {
    let pair = pair();
    let mut follower = pair.on_the_follower("narrow");
    let mut whole = pair.on_the_leader("narrow");
    for read in [
        "SELECT * FROM ledger;",
        "SELECT * FROM ledger WHERE total > 0;",
        "SELECT count(*) AS n FROM ledger WHERE total > 0;",
    ] {
        assert_eq!(
            answer(&mut follower, read).0,
            answer(&mut whole, read).0,
            "{read}"
        );
    }
    let (records, _) = answer(&mut follower, "SELECT * FROM ledger;");
    assert_eq!(records.len(), 7);
    assert!(
        records
            .iter()
            .all(|record| !format!("{record:?}").contains("total")),
        "{records:?}"
    );
    let (matched, _) = answer(&mut follower, "SELECT * FROM ledger WHERE total > 0;");
    assert!(matched.is_empty(), "{matched:?}");
}
