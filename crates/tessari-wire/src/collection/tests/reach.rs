use super::*;

#[test]
fn a_node_nobody_subscribed_is_refused_the_log_it_asks_for() {
    // C-37's own cheapest decisive test, and the low-privilege probe the
    // access-control discipline asks for: a node with a credential this
    // cluster issued, proven at the door, asking directly over the protocol
    // with nothing else in the loop.
    let authority = Authority::new();
    let leader = granting("");
    let (address, door) = declaring(&authority, &leader);

    let refused = collect(&authority, address, 1, 64)
        .expect_err("a peer nobody subscribed may not take the log");
    door.join().expect("the door's thread");

    // The refusal it was, not a closed socket: a node whose connection
    // ended mid-frame would be looking for a network fault instead of
    // reading the one sentence that says what to do.
    assert!(
        matches!(refused, Error::Unsubscribed),
        "a refusal, and not the same one a stranded follower gets: {refused}"
    );
    let said = refused.to_string();
    assert!(
        said.contains("DEFINE REPLICA") && said.contains("REPLICATES"),
        "and it names the statement that grants one: {said}"
    );
}

#[test]
fn an_answer_says_what_it_was_served_under_and_an_older_answer_says_nothing() {
    use tessari_types::{DatabaseId, NamespaceId, ShardId, TableId};
    let shard = Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        ShardId::new(4),
    );
    let answer = Collected {
        log: LogId::unattributed(Reach::Store),
        previous: Epoch::ZERO,
        records: Vec::new(),
        stopped_early: false,
        over: Some(shard),
        order: None,
        epoch: None,
    };
    let encoded = answer.encode();
    assert_eq!(
        Collected::decode(&encoded).expect("an answer").over,
        Some(shard)
    );
    // The same answer as a leader that predates the field wrote it: the
    // body ends at the flag, and that reads as not stated.
    let older = &encoded[..encoded.len() - 17];
    assert_eq!(
        Collected::decode(older).expect("an older answer").over,
        None
    );
    // G034 — the leader's order travels after `over`, and a body that ends
    // at `over` (a leader that predates it) reads as not stated.
    let ordered = Collected {
        order: Some(Sequence::new(42)),
        ..answer
    };
    let bytes = ordered.encode();
    let back = Collected::decode(&bytes).expect("an ordered answer");
    assert_eq!(back.order, Some(Sequence::new(42)));
    assert_eq!(back.over, Some(shard));
    assert_eq!(
        Collected::decode(&bytes[..bytes.len() - 8])
            .expect("an answer without an order")
            .order,
        None
    );
    // ADR-0107 — the leadership the order counts under travels after it,
    // and a body that ends at the order reads as not stated.
    let stated = Collected {
        epoch: Some(Epoch::new(9)),
        ..ordered
    };
    let bytes = stated.encode();
    let back = Collected::decode(&bytes).expect("an answer stating its leadership");
    assert_eq!(back.epoch, Some(Epoch::new(9)));
    assert_eq!(back.order, Some(Sequence::new(42)));
    assert_eq!(
        Collected::decode(&bytes[..bytes.len() - 8])
            .expect("an answer from a leader that predates it")
            .epoch,
        None
    );
}

#[test]
fn a_follower_asks_only_for_logs_its_served_reach_touches() {
    let db = Db::in_memory().expect("an in-memory store");
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT 'g';",
        )
        .expect("a split table");
    let every = logs_to_collect(db.store()).expect("this node's own logs");
    let second = every
        .iter()
        .copied()
        .find(|home| matches!(home, Reach::Shard(_, _, _, shard) if shard.get() == 2))
        .expect("shard 2 is a log");
    db.store().record_served(second).expect("recorded");
    let narrowed = logs_to_collect(db.store()).expect("this node's own logs");
    assert!(narrowed.contains(&second));
    assert!(narrowed.contains(&Reach::Store), "the chain above it stays");
    assert!(
        !narrowed
            .iter()
            .any(|home| matches!(home, Reach::Shard(_, _, _, shard) if shard.get() == 1)),
        "the sibling shard is not asked for: {narrowed:?}"
    );
}

#[test]
fn a_follower_still_asks_for_a_shard_a_split_retired() {
    // ADR-0095 D3: a retired shard's log holds what was written to it before
    // the split, and a follower that had not collected all of it yet when it
    // applied the split would otherwise never ask for the rest.
    let db = Db::in_memory().expect("an in-memory store");
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT 'g'; \
                 ALTER TABLE orders SPLIT AT 'm';",
        )
        .expect("a split table, split again");
    let shards: Vec<u32> = logs_to_collect(db.store())
        .expect("this node's own logs")
        .into_iter()
        .filter_map(|home| match home {
            Reach::Shard(_, _, _, shard) => Some(shard.get()),
            _ => None,
        })
        .collect();
    assert_eq!(
        shards,
        vec![1, 2, 3, 4],
        "shard 2 is retired and still a log"
    );
}

/// G034 S1.2 over the peer door (Q-796): a record written by a
/// one-database commit and then by a two-database one ends at the leader's
/// value on a follower that collects every log in one round. Collected a
/// log at a time, the namespace log's later commit was applied first and the
/// database log's earlier one last.
#[test]
fn a_round_applies_a_writers_logs_in_the_order_it_committed_them() {
    let authority = Authority::new();
    let leader = granting(" REPLICATES NAMESPACE prod");
    leader
        .session()
        .run(
            "USE NAMESPACE prod; DEFINE DATABASE notes; USE DATABASE notes; \
                 DEFINE COLLECTION pad; CREATE pad:1 = { n: 0 }; \
                 USE DATABASE orders; UPDATE users:1 MERGE { n: 3 }; \
                 BEGIN; UPDATE users:1 MERGE { n: 4 }; USE DATABASE notes; \
                 UPDATE pad:1 MERGE { n: 1 }; COMMIT;",
        )
        .expect("the leader's writes");
    // The store's log, then the four logs it makes known in one round.
    let (address, door) = declaring_for(&authority, &leader, 5);
    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let collector = collector(&mine, &said, address, 1024);
    let store = collector
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("the store's log");
    let logs = logs_to_collect(follower.store()).expect("this node's own logs");
    assert_eq!(
        logs.len(),
        4,
        "store, namespace and two databases: {logs:?}"
    );
    let asks: Vec<(Reach, Sequence)> = logs
        .iter()
        .map(|home| {
            let from = if *home == Reach::Store {
                Sequence::new(store.get() + 1)
            } else {
                Sequence::new(1)
            };
            (*home, from)
        })
        .collect();
    for reached in collector.round(follower.store(), &asks) {
        reached.expect("every log collects");
    }
    door.join().expect("the door's thread");
    let read = |db: &Db| {
        let outcomes = db
            .session()
            .run("USE NAMESPACE prod; USE DATABASE orders; SELECT n FROM users:1;")
            .expect("a read");
        format!("{:?}", outcomes.last())
    };
    assert!(read(&leader).contains("Integer(4)"), "{}", read(&leader));
    assert_eq!(read(&follower), read(&leader));
}

/// G050 C2 (ADR-0095): writers keep committing while the table is split and
/// then merged; every write acknowledged is on the leader exactly once, and a
/// follower collecting over the peer door ends holding the same records and
/// the same map.
#[test]
fn writes_across_a_split_and_a_merge_are_all_kept_and_all_collected() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WRITERS: usize = 4;
    const EACH: usize = 60;
    /// Generous: one per fetch, and the walk needs a few passes over ~10 logs.
    const ROUNDS: usize = 160;

    let authority = Authority::new();
    let leader = granting(" REPLICATES STORE");
    leader
        .session()
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; \
                 DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'm';",
        )
        .expect("a split table");

    let written = AtomicUsize::new(0);
    let acknowledged: BTreeSet<String> = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let (leader, written) = (&leader, &written);
                scope.spawn(move || {
                    let mut session = leader.session();
                    session
                        .run("USE NAMESPACE prod; USE DATABASE orders;")
                        .expect("tenancy");
                    let mut kept = Vec::new();
                    for n in 0..EACH {
                        // Spread over the whole key range, so every shard
                        // — retired, minted and merged — takes writes.
                        let letter = char::from(
                            b'a' + u8::try_from((n * 7 + writer) % 26).expect("a letter"),
                        );
                        let id = format!("{letter}{writer}{n:03}");
                        let statement = format!("CREATE ledger:'{id}' = {{ n: {n} }};");
                        for _ in 0..8 {
                            if session.run(&statement).is_ok() {
                                kept.push(id.clone());
                                break;
                            }
                        }
                        written.fetch_add(1, Ordering::Relaxed);
                    }
                    kept
                })
            })
            .collect();
        let mut changes = leader.session();
        changes
            .run("USE NAMESPACE prod; USE DATABASE orders;")
            .expect("tenancy");
        while written.load(Ordering::Relaxed) < 40 {
            std::thread::yield_now();
        }
        // Shard 1 (before 'm') into 3 and 4, while the writers write.
        changes
            .run("ALTER TABLE ledger SPLIT AT 'f';")
            .expect("the split commits");
        while written.load(Ordering::Relaxed) < 120 {
            std::thread::yield_now();
        }
        // 4 ('f'..'m') and 2 ('m'..) into 5.
        changes
            .run("ALTER TABLE ledger MERGE SHARD 4, 2;")
            .expect("the merge commits");
        writers
            .into_iter()
            .flat_map(|writer| writer.join().expect("a writer"))
            .collect()
    });
    assert!(
        acknowledged.len() > WRITERS * EACH / 2,
        "most writes are acknowledged: {}",
        acknowledged.len()
    );

    let (address, door) = declaring_for(&authority, &leader, ROUNDS);
    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let collector = collector(&mine, &said, address, 1024);
    let mut reached: BTreeMap<Reach, Sequence> = BTreeMap::new();
    let mut used = 0_usize;
    let mut settled = false;
    for _ in 0..12 {
        let logs = logs_to_collect(follower.store()).expect("this node's own logs");
        let asks: Vec<(Reach, Sequence)> = logs
            .iter()
            .map(|home| {
                let at = reached.get(home).copied().unwrap_or(Sequence::ZERO);
                (*home, Sequence::new(at.get() + 1))
            })
            .collect();
        used += asks.len();
        assert!(
            used <= ROUNDS,
            "the walk needs more rounds than the door serves"
        );
        let mut moved = false;
        for ((home, _), answer) in asks.iter().zip(collector.round(follower.store(), &asks)) {
            if let Ok(at) = answer
                && reached.get(home) != Some(&at)
            {
                reached.insert(*home, at);
                moved = true;
            }
        }
        let again = logs_to_collect(follower.store()).expect("this node's own logs");
        if !moved && again == logs {
            settled = true;
            break;
        }
    }
    // The door serves a fixed number of connections and the walk's count
    // is not worth predicting: dial the store's log until it has served
    // them all, never more than it was told to.
    for _ in 0..ROUNDS {
        if door.is_finished() {
            break;
        }
        // From where the follower stands: an ask from the first position
        // is refused before it dials on a store that already holds one.
        let at = reached
            .get(&Reach::Store)
            .copied()
            .unwrap_or(Sequence::ZERO);
        drop(collector.collect(follower.store(), Reach::Store, Sequence::new(at.get() + 1)));
    }
    door.join().expect("the door's thread");
    assert!(settled, "the follower never caught up: {reached:?}");

    let read = |db: &Db, script: &str| -> Outcome {
        let mut outcomes = db
            .session()
            .run(&format!(
                "USE NAMESPACE prod; USE DATABASE orders; {script}"
            ))
            .expect("a read");
        outcomes.pop().expect("an answer")
    };
    let ids = |db: &Db| -> Vec<String> {
        let records = match read(db, "SELECT n FROM ledger;") {
            Outcome::Records { records, .. } => Some(records),
            _ => None,
        }
        .expect("a read answers records");
        records
            .into_iter()
            .map(|(id, _)| {
                match id {
                    tessari_types::RecordId::Text(id) => Some(id),
                    _ => None,
                }
                .expect("the ids here are text")
            })
            .collect()
    };
    let on_the_leader = ids(&leader);
    assert_eq!(
        on_the_leader.len(),
        acknowledged.len(),
        "a write was lost or kept twice"
    );
    assert_eq!(
        on_the_leader.iter().cloned().collect::<BTreeSet<_>>(),
        acknowledged,
        "the leader's records are not the acknowledged writes"
    );
    assert_eq!(
        ids(&follower),
        on_the_leader,
        "the follower does not hold what the leader holds"
    );
    let map = |db: &Db| format!("{:?}", read(db, "INFO FOR TABLE ledger;"));
    assert!(
        map(&leader).contains("\"version\": Number(Integer(2))"),
        "{}",
        map(&leader)
    );
    assert_eq!(
        map(&follower),
        map(&leader),
        "the follower holds a different map"
    );
}

#[test]
fn a_split_tables_shards_are_logs_a_follower_asks_for_after_its_database() {
    let db = Db::in_memory().expect("an in-memory store");
    db.session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT 'g';",
        )
        .expect("a split table");
    let logs = logs_to_collect(db.store()).expect("this node's own logs");
    let at = logs
        .iter()
        .position(|home| matches!(home, Reach::Database(..)))
        .expect("the database is a log");
    let after: Vec<Option<u32>> = logs[at + 1..]
        .iter()
        .map(|home| match home {
            Reach::Shard(_, _, _, shard) => Some(shard.get()),
            _ => None,
        })
        .collect();
    let shards: Vec<u32> = after.iter().flatten().copied().collect();
    assert_eq!(
        after.len(),
        shards.len(),
        "only the table's shards follow its database: {logs:?}"
    );
    assert_eq!(shards, vec![1, 2]);
}
