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
use tessari_session::{Asked, Gather, Gathered, Note, Outcome, Reduced, Session, Unanswered};
use tessari_storage::{Reach, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, ShardId, TableId, Value};

const PASSWORD: &str = "correct horse battery";
const A_FOLLOWER: [u8; 16] = [7; 16];
const THE_LEADER: [u8; 16] = [9; 16];

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

pub(crate) fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// A leader holding `ledger` split at 'g' and 'p', with records in all three
/// shards, an owner, a reader, a reader who may see only `note`, and a node.
pub(crate) fn leader() -> Arc<Store> {
    let leader = store();
    Session::new(&leader)
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE ledger (total int, note string, peer record) IDENTITY uuid \
             SPLIT AT 'g', 'p';\n\
             DEFINE TABLE other (total int) IDENTITY uuid;\n\
             DEFINE TABLE points (at vector<2>) IDENTITY uuid SPLIT AT 'g', 'p';\n\
             CREATE points:'a' = { at: [1, 1] }; CREATE points:'b' = { at: [5, 5] };\n\
             CREATE points:'h' = { at: [2, 2] }; CREATE points:'k' = { at: [6, 6] };\n\
             CREATE points:'q' = { at: [3, 3] }; CREATE points:'r' = { at: [3, 3] };\n\
             CREATE points:'z' = { at: [9, 9] };\n\
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

fn table(store: &Store, name: &str) -> TableId {
    let mut transaction = store.begin().unwrap();
    tessari_storage::Catalog::new(&mut transaction)
        .table_id(NamespaceId::new(1), DatabaseId::new(1), name)
        .unwrap()
        .unwrap()
}

fn shard(store: &Store, id: u32) -> Reach {
    shard_of(store, "ledger", id)
}

fn shard_of(store: &Store, name: &str, id: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        table(store, name),
        ShardId::new(id),
    )
}

/// A follower of shard 2 only, recorded as served that way.
pub(crate) fn follower_of_the_middle(leader: &Store) -> Store {
    follower_of(leader, shard(leader, 2))
}

/// A follower of the middle shard of `name`, recorded as served that way.
pub(crate) fn follower_of_the_middle_of(leader: &Store, name: &str) -> Store {
    follower_of(leader, shard_of(leader, name, 2))
}

fn follower_of(leader: &Store, over: Reach) -> Store {
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
    /// The condition each question carried, in the order asked (ADR-0097).
    pushed: Mutex<Vec<Option<tessari_session::Pushed>>>,
    /// How many records were sent rather than folded (ADR-0097 D2).
    sent: Mutex<usize>,
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
        self.pushed.lock().unwrap().push(asked.pushed.cloned());
        let transaction = self.leader.begin().unwrap();
        let records = transaction
            .records_between(
                asked.namespace,
                asked.database,
                asked.table,
                asked.window,
                None,
                match (asked.reduce, asked.counting) {
                    (None, None) => asked.most.saturating_add(1),
                    _ => usize::MAX,
                },
            )
            .unwrap();
        // Folded as the peer door folds, in one page.
        if let Some(reduce) = asked.reduce {
            let reduced = match tessari_session::reducing(&self.leader, reduce, records).unwrap() {
                Some(partials) => Reduced::Partials(partials),
                None => Reduced::Declined,
            };
            return Ok(Gathered {
                records: Vec::new(),
                node: THE_LEADER,
                reduced: Some(reduced),
                counted: None,
            });
        }
        // Counted as the peer door counts, through the analysis the leader's
        // own index writer uses (ADR-0103).
        if let Some(counting) = asked.counting {
            let mut transaction = self.leader.begin().unwrap();
            let index = tessari_storage::Catalog::new(&mut transaction)
                .indexes_on(asked.table)
                .unwrap()
                .into_iter()
                .find(|index| index.id == counting.index)
                .unwrap();
            let counted = transaction
                .search_counts(&index, &records, &counting.terms)
                .unwrap();
            return Ok(Gathered {
                records: Vec::new(),
                node: THE_LEADER,
                reduced: None,
                counted: Some(counted),
            });
        }
        // Narrowed as the peer door narrows, so every equality below runs
        // with the leader's half of the pushdown in place.
        let records = match asked.pushed {
            Some(pushed) => tessari_session::keeping(&self.leader, pushed, records).unwrap(),
            None => records,
        };
        // Ranked as the peer door ranks, so only the shard's first `n` is sent
        // (ADR-0102).
        let records = match asked.ordered {
            Some(ordered) => {
                let mut page = Some(records);
                tessari_session::leading(&self.leader, ordered, || Ok(page.take())).unwrap()
            }
            None => records,
        };
        if records.len() > asked.most {
            return Err(Unanswered::Ceiling);
        }
        let mut sent = self.sent.lock().unwrap();
        *sent = sent.saturating_add(records.len());
        drop(sent);
        Ok(Gathered {
            records,
            node: THE_LEADER,
            reduced: None,
            counted: None,
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

/// A gatherer whose leader holds a different map of the table.
#[derive(Debug)]
pub(crate) struct Moved;

impl Gather for Moved {
    fn gather(&self, _: &Asked<'_>) -> Result<Gathered, Unanswered> {
        Err(Unanswered::Moved)
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
            reduced: None,
            counted: None,
        })
    }
}

pub(crate) struct Pair {
    leader: Arc<Store>,
    follower: Store,
    gatherer: Arc<FromTheLeader>,
}

pub(crate) fn pair() -> Pair {
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    pair_of(leader, follower)
}

/// A leader and a follower of part of it, the follower gathering from it.
pub(crate) fn pair_of(leader: Arc<Store>, follower: Store) -> Pair {
    let gatherer = Arc::new(FromTheLeader {
        leader: Arc::clone(&leader),
        asked: Mutex::new(Vec::new()),
        pushed: Mutex::new(Vec::new()),
        sent: Mutex::new(0),
    });
    Pair {
        leader,
        follower,
        gatherer,
    }
}

impl Pair {
    pub(crate) fn on_the_follower(&self, user: &str) -> Session<'_> {
        let mut session = signed_in(&self.follower, user)
            .gathering(Arc::clone(&self.gatherer) as Arc<dyn Gather>);
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        session
    }

    pub(crate) fn on_the_leader(&self, user: &str) -> Session<'_> {
        let mut session = signed_in(&self.leader, user);
        session
            .run("USE NAMESPACE prod; USE DATABASE shop;")
            .unwrap();
        session
    }

    pub(crate) fn leader(&self) -> &Store {
        &self.leader
    }

    fn asked(&self) -> Vec<Question> {
        std::mem::take(&mut *self.gatherer.asked.lock().unwrap())
    }

    /// How many records travelled since this was last asked.
    pub(crate) fn sent(&self) -> usize {
        std::mem::take(&mut *self.gatherer.sent.lock().unwrap())
    }
}

pub(crate) fn answer(session: &mut Session<'_>, read: &str) -> (Vec<(RecordId, Value)>, Vec<Note>) {
    match session.run(read) {
        Ok(outcomes) => match outcomes.last() {
            Some(Outcome::Records { records, notes, .. }) => (records.clone(), notes.clone()),
            other => panic!("{read}: {other:?}"),
        },
        Err(error) => panic!("{read}: {error:?}"),
    }
}

pub(crate) fn refused(session: &mut Session<'_>, read: &str) -> tessari_session::Error {
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
        "SELECT * FROM ledger LIMIT 2;",
        "SELECT * FROM ledger START 2 LIMIT 3;",
        "SELECT * FROM ledger WHERE total > 2 LIMIT 3;",
        "SELECT * FROM ledger LIMIT 6;",
    ]
    .into_iter()
    .chain(FOLDED)
    {
        let (gathered, _) = answer(&mut follower, read);
        let (expected, _) = answer(&mut whole, read);
        assert!(!expected.is_empty(), "{read}: the control answered nothing");
        assert_eq!(gathered, expected, "{read}");
    }
}

/// Grouping reads whose every fold merges exactly, so the leaders fold them.
const FOLDED: [&str; 7] = [
    "SELECT count(*) AS n FROM ledger;",
    "SELECT sum(total) AS sum, note FROM ledger GROUP BY note;",
    "SELECT count(*) AS n, min(total) AS low, max(total) AS high, mean(total) AS avg FROM ledger;",
    "SELECT note, count(*) AS n FROM ledger WHERE total > 2 GROUP BY note;",
    "SELECT mean(total) * 2 AS twice FROM ledger;",
    "SELECT min(note) AS first, max(note) AS last FROM ledger;",
    "SELECT count(peer) AS linked FROM ledger;",
];

/// ADR-0097 D2 — a grouping read is folded on the leaders, so no record of a
/// shard this node lacks travels, and the answer is still the whole node's.
#[test]
fn a_grouping_read_is_folded_on_the_leader_and_no_record_travels() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    for read in FOLDED {
        let (gathered, notes) = answer(&mut follower, read);
        assert_eq!(gathered, answer(&mut whole, read).0, "{read}");
        assert_eq!(pair.sent(), 0, "{read}: records travelled");
        assert!(
            notes.iter().any(|note| note.kind() == "gathered"),
            "{read}: {notes:?}"
        );
    }
    // A fold that does not merge exactly gathers the records, as before.
    let read = "SELECT median(total) AS middle FROM ledger;";
    assert_eq!(answer(&mut follower, read).0, answer(&mut whole, read).0);
    assert_eq!(pair.sent(), 5, "{read}");
    // A float offered to `sum` declines the fold, because float addition depends
    // on its order; the read gathers records and answers the whole node's total.
    let read = "SELECT sum(total * 0.5) AS half FROM ledger;";
    assert_eq!(answer(&mut follower, read).0, answer(&mut whole, read).0);
    assert_eq!(pair.sent(), 5, "{read}");
}

/// A leader that folded more records than a gather may hold into one group.
#[derive(Debug)]
struct FoldingMany;

impl FoldingMany {
    /// How many records each shard folded: one past the ceiling.
    fn many() -> i64 {
        i64::try_from(tessari_constants::GATHER_RECORDS + 1).unwrap()
    }
}

impl Gather for FoldingMany {
    fn gather(&self, asked: &Asked<'_>) -> Result<Gathered, Unanswered> {
        // Asked for the records themselves, there are too many to send.
        if asked.reduce.is_none() {
            return Err(Unanswered::Ceiling);
        }
        let many = Self::many();
        let total = tessari_types::Number::from(many).as_decimal().unwrap();
        Ok(Gathered {
            records: Vec::new(),
            node: THE_LEADER,
            counted: None,
            reduced: Some(Reduced::Partials(vec![tessari_session::Partial {
                key: vec![Value::from("many")],
                first: asked
                    .window
                    .from
                    .cloned()
                    .unwrap_or_else(|| RecordId::from("a")),
                states: vec![
                    Value::from(many),
                    Value::Array(vec![
                        Value::Number(tessari_types::Number::Decimal(total)),
                        Value::Bool(true),
                    ]),
                ],
            }])),
        })
    }
}

/// ADR-0097 D3 — the ceiling is on what travels: a fold over shards holding
/// more records than a gather may hold answers, and a read of those records is
/// still refused rather than answered in part.
#[test]
fn a_fold_over_more_records_than_a_gather_holds_answers() {
    let pair = pair();
    let mut follower = signed_in(&pair.follower, "reader").gathering(Arc::new(FoldingMany));
    follower
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let (rows, _) = answer(
        &mut follower,
        "SELECT note, count(*) AS n, sum(total) AS sum FROM ledger GROUP BY note;",
    );
    // Shards 1 and 3 each folded `many`; this node's own shard 2 holds h and k.
    let both = Value::from(FoldingMany::many() * 2);
    let many = rows
        .iter()
        .map(|(_, row)| row)
        .find(|row| format!("{row:?}").contains("many"))
        .expect("the folded group");
    let Value::Object(fields) = many else {
        panic!("{many:?}")
    };
    assert_eq!(fields.get("n"), Some(&both), "{many:?}");
    assert_eq!(fields.get("sum"), Some(&both), "{many:?}");
    assert_eq!(rows.len(), 3, "{rows:?}");
    match refused(&mut follower, "SELECT * FROM ledger;") {
        tessari_session::Error::GatheredTooMuch { table, .. } => assert_eq!(table, "ledger"),
        other => panic!("expected GatheredTooMuch, got {other:?}"),
    }
}

/// A leader that folds as [`FoldingMany`] does, slowly.
#[derive(Debug)]
struct FoldingSlowly;

impl Gather for FoldingSlowly {
    fn gather(&self, asked: &Asked<'_>) -> Result<Gathered, Unanswered> {
        std::thread::sleep(std::time::Duration::from_millis(200));
        FoldingMany.gather(asked)
    }
}

/// Q-856 (G053 SG8): a folded read keeps its `TIMEOUT` while it waits on the
/// leaders' folds — refused, never answered late as if nothing was asked.
#[test]
fn a_folded_read_past_its_timeout_is_refused_between_the_leaders_folds() {
    let pair = pair();
    let mut follower = signed_in(&pair.follower, "reader").gathering(Arc::new(FoldingSlowly));
    follower
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    match refused(
        &mut follower,
        "SELECT note, count(*) AS n, sum(total) AS sum FROM ledger GROUP BY note TIMEOUT 50ms;",
    ) {
        tessari_session::Error::TimedOut { after, .. } => assert_eq!(after, "50ms"),
        other => panic!("expected TimedOut, got {other:?}"),
    }
    // Within its ceiling, the same read answers.
    let (rows, _) = answer(
        &mut follower,
        "SELECT note, count(*) AS n, sum(total) AS sum FROM ledger GROUP BY note TIMEOUT 1h;",
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
}

/// G050 C4, end to end: a shard holding one record past the ceiling, folded by
/// the real leader-side code and merged here.
#[test]
#[ignore = "inserts 100 001 records — about three minutes in a debug build; \
            G050 C4's own validation, run explicitly: cargo test -p tessari-session \
            --test suite a_fold_over_a_real_shard_past_the_ceiling -- --ignored"]
fn a_fold_over_a_real_shard_past_the_ceiling_answers() {
    let pair = pair();
    let past = tessari_constants::GATHER_RECORDS + 1;
    // Generated identities sort after every text one, so all of these land in
    // shard 3, which this node lacks.
    let mut insert = String::from("INSERT INTO ledger (total, note) VALUES ");
    for n in 0..past {
        if n > 0 {
            insert.push_str(", ");
        }
        insert.push_str("(1, 'many')");
    }
    insert.push(';');
    pair.on_the_leader("root").run(&insert).unwrap();
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let read = "SELECT note, count(*) AS n, sum(total) AS sum FROM ledger GROUP BY note;";
    let (gathered, _) = answer(&mut follower, read);
    assert_eq!(gathered, answer(&mut whole, read).0);
    assert_eq!(pair.sent(), 0, "records travelled");
    let many = gathered
        .iter()
        .find(|(_, row)| format!("{row:?}").contains("many"))
        .expect("the inserted group");
    assert!(
        format!("{:?}", many.1).contains(&past.to_string()),
        "{many:?}"
    );
    match refused(&mut follower, "SELECT * FROM ledger;") {
        tessari_session::Error::GatheredTooMuch { table, .. } => assert_eq!(table, "ledger"),
        other => panic!("expected GatheredTooMuch, got {other:?}"),
    }
}

/// ADR-0096 D3 — a read naming a partition asks only for the shard holding
/// it, and only for that partition's identities.
#[test]
fn a_read_naming_a_partition_asks_only_its_shard() {
    // Declared beside `ledger` before the follower is copied, so it knows the
    // table; it was served only `ledger`'s middle shard, so it holds none of
    // `customers` and gathers every read of it.
    let leader = leader();
    signed_in(&leader, "root")
        .run(
            "USE NAMESPACE prod; USE DATABASE shop;\n\
             DEFINE TABLE customers (region string, name string) IDENTITY uuid \
             PARTITION BY region SPLIT AT 'de', 'fr';\n\
             CREATE customers:'at:1' = { region: 'at', name: 'cy' };\n\
             CREATE customers:'de:1' = { region: 'de', name: 'ada' };\n\
             CREATE customers:'fr:1' = { region: 'fr', name: 'bo' };",
        )
        .unwrap();
    let follower = follower_of_the_middle(&leader);
    let gatherer = Arc::new(FromTheLeader {
        leader: Arc::clone(&leader),
        asked: Mutex::new(Vec::new()),
        pushed: Mutex::new(Vec::new()),
        sent: Mutex::new(0),
    });
    let pair = Pair {
        leader,
        follower,
        gatherer,
    };
    let mut follower = pair.on_the_follower("reader");
    let mut whole = pair.on_the_leader("reader");
    let read = "SELECT * FROM customers WHERE region = 'de';";
    let (gathered, _) = answer(&mut follower, read);
    assert_eq!(gathered, answer(&mut whole, read).0);
    assert_eq!(gathered.len(), 1, "{gathered:?}");
    let id = |text: &str| RecordId::from(text);
    assert_eq!(
        pair.asked(),
        vec![(2, Some(id("de:")), Some((id("de;"), false)))]
    );
}

/// ADR-0097 D2 — a bounded read stops asking once it has enough: shard 1 holds
/// three records, this node holds shard 2's two, and shard 3 is asked only when
/// those five are not enough.
#[test]
fn a_bounded_read_asks_no_shard_it_does_not_need() {
    let pair = pair();
    let mut follower = pair.on_the_follower("reader");
    let shards = |pair: &Pair| -> Vec<u32> {
        pair.asked()
            .into_iter()
            .map(|(shard, _, _)| shard)
            .collect()
    };
    answer(&mut follower, "SELECT * FROM ledger LIMIT 2;");
    assert_eq!(shards(&pair), vec![1]);
    answer(&mut follower, "SELECT * FROM ledger LIMIT 5;");
    assert_eq!(shards(&pair), vec![1]);
    answer(&mut follower, "SELECT * FROM ledger LIMIT 6;");
    assert_eq!(shards(&pair), vec![1, 3]);
    // A condition that stays home cannot bound what the leader sends.
    answer(
        &mut follower,
        "SELECT * FROM ledger WHERE note != rand::uuid() LIMIT 2;",
    );
    assert_eq!(shards(&pair), vec![1, 3]);
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
        tessari_session::Error::NotHeldHere { table, shards, .. } => (table, shards),
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

    // ADR-0095 D4: a leader holding a different map is a reason to read the map
    // again, and is said as one rather than as a leader that did not answer.
    let mut moved = signed_in(&pair.follower, "reader").gathering(Arc::new(Moved));
    moved.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    match refused(&mut moved, "SELECT * FROM ledger;") {
        tessari_session::Error::ShardMapMoved { table, shard, .. } => {
            assert_eq!((table.as_str(), shard), ("ledger", 1));
        }
        other => panic!("expected ShardMapMoved, got {other:?}"),
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
        "SELECT count(total) AS seen, sum(total) AS sum, count(*) AS every FROM ledger;",
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

/// ADR-0097 — a condition that reads only the record travels, with the
/// session's visibility and no value written into it; one that reads the
/// clock or the generator stays home.
#[test]
fn a_condition_travels_to_the_leader_with_the_askers_visibility() {
    let pair = pair();
    let pushed = |user: &str, read: &str| {
        let mut follower = pair.on_the_follower(user);
        drop(follower.run(read));
        let mut sent = std::mem::take(&mut *pair.gatherer.pushed.lock().unwrap());
        sent.pop().expect("the read gathered")
    };
    let full = pushed("reader", "SELECT * FROM ledger WHERE total > 2;").expect("pushed");
    assert_eq!(full.visible, None);
    assert!(
        !full.condition.contains('2'),
        "a value became text: {}",
        full.condition
    );
    assert_eq!(
        full.parameters.values().collect::<Vec<_>>(),
        vec![&Value::from(2_i64)]
    );
    let narrow = pushed("narrow", "SELECT * FROM ledger WHERE total > 2;").expect("pushed");
    assert!(
        narrow
            .visible
            .as_ref()
            .is_some_and(|fields| !fields.contains("total")),
        "{:?}",
        narrow.visible
    );
    assert_eq!(
        pushed("reader", "SELECT * FROM ledger WHERE note = rand::uuid();"),
        None,
        "a condition that reads the generator was pushed"
    );
}

/// A follower of the whole database, recorded as served that way.
fn follower_of_the_database(leader: &Store) -> Store {
    let over = Reach::Database(NamespaceId::new(1), DatabaseId::new(1));
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

#[test]
fn a_node_holding_part_of_a_split_table_refuses_to_back_it_up() {
    // A backup assembled from what one node happens to hold is not one state of
    // anything (ADR-0094 D7): it is refused, naming the shards it lacks.
    let leader = leader();
    let follower = follower_of_the_middle(&leader);
    for statement in [
        "BACKUP STATE;",
        "BACKUP STATE OF prod.shop;",
        "BACKUP OF NAMESPACE prod;",
    ] {
        match signed_in(&follower, "root").run(statement) {
            Err(tessari_session::Error::NotHeldHere { table, shards, .. }) => {
                assert_eq!(
                    (table.as_str(), shards),
                    ("ledger", vec![1, 3]),
                    "{statement}"
                );
            }
            Ok(_) => panic!("{statement} on a partial holder answered with a backup"),
            Err(other) => panic!("{statement} on a partial holder was refused as {other:?}"),
        }
    }
}

#[test]
fn a_node_holding_every_shard_backs_the_database_up_complete() {
    let leader = leader();
    let follower = follower_of_the_database(&leader);
    let Some(tessari_session::Outcome::Value(Value::Bytes(file))) = signed_in(&follower, "root")
        .run("BACKUP STATE OF prod.shop;")
        .unwrap()
        .pop()
    else {
        panic!("a snapshot is bytes");
    };
    // Each shard's log is recorded at its own position.
    let taken = tessari_backup::verify_state(&mut file.as_slice()).unwrap();
    let shards = leader
        .logs()
        .unwrap()
        .into_iter()
        .filter(|log| matches!(log.home, Reach::Shard(..)))
        .count();
    // Two split tables, `ledger` and `points`, of three shards each.
    assert_eq!(shards, 6, "the leader keeps a log per shard");
    let recorded = taken
        .positions
        .iter()
        .filter(|(log, _)| matches!(log.home, Reach::Shard(..)))
        .count();
    assert_eq!(recorded, shards, "{:?}", taken.positions);

    let restored = store();
    tessari_backup::read_state(&restored, || Ok(std::io::Cursor::new(file.clone()))).unwrap();
    let read = "USE NAMESPACE prod; USE DATABASE shop; SELECT note FROM ledger;";
    assert_eq!(
        format!("{:?}", signed_in(&restored, "root").run(read).unwrap()),
        format!("{:?}", signed_in(&leader, "root").run(read).unwrap()),
        "the restored database is not the leader's"
    );
}
