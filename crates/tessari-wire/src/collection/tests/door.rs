use super::*;

#[test]
fn a_door_places_a_candidate_only_where_its_own_row_leads() {
    // ADR-0098. The voter's half of a placement move: the node's catalog,
    // read by the rule a candidate stands by.
    let db = granting(" LEADS NAMESPACE prod");
    use super::super::Origin;
    let serving = Serving::declared(db.store());
    let mut transaction = db.store().begin().expect("a transaction");
    let declared = Catalog::new(&mut transaction).replicas().expect("the rows");
    transaction.rollback();
    let placed = declared[0].leads.expect("the row places a namespace");
    assert!(serving.places(THERE, placed));
    assert!(!serving.places(LEADER, placed), "a node no row places");
    assert!(!serving.places(THERE, Reach::Store));
}

/// The door the node actually runs — `Peers::serve`, on a runtime — over
/// the same store; the tests above drive the synchronous door that shares
/// its answering with it.
struct OnTheRuntime {
    db: Arc<Db>,
}

impl super::super::Origin for OnTheRuntime {
    fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
        Serving::declared(self.db.store()).collected(follower, asked)
    }

    fn gathered(
        &self,
        asker: [u8; NODE_ID_LEN],
        asked: &crate::gathering::Gather,
    ) -> Result<crate::gathering::Page> {
        Serving::declared(self.db.store()).gathered(asker, asked)
    }

    fn places(&self, candidate: [u8; NODE_ID_LEN], range: Reach) -> bool {
        Serving::declared(self.db.store()).places(candidate, range)
    }

    fn copied(
        &self,
        follower: [u8; NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        Serving::declared(self.db.store()).copied(follower, write)
    }
}

impl crate::door::Holding for OnTheRuntime {
    fn hello(&self) -> Result<crate::peer::Hello> {
        Ok(hello(LEADER))
    }

    fn met(&self, _: &crate::link::Met) {}

    fn commits(&self) -> tokio::sync::watch::Receiver<u64> {
        self.db.commits().watching()
    }

    fn coordinated(
        &self,
        _: [u8; NODE_ID_LEN],
        _: &crate::assertion::Assertion,
        _: &crate::coordination::Coordinate,
    ) -> std::result::Result<tessaridb::Coordinated, String> {
        Err("this test door carries no requests".to_owned())
    }

    fn across(
        &self,
        _: [u8; NODE_ID_LEN],
        _: &crate::assertion::Assertion,
        _: &[u8],
    ) -> std::result::Result<Vec<u8>, tessari_session::PartRefused> {
        Err(tessari_session::PartRefused {
            kind: tessari_session::RefusalKind::Invalid,
            reason: "this test door writes no cross-leader records".to_owned(),
        })
    }
}

#[test]
fn a_peer_nobody_subscribed_is_refused_by_name_by_the_door_on_the_runtime() {
    let authority = Authority::new();
    let leader = granting("");
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(LEADER, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the door");
    let stop = tokio_util::sync::CancellationToken::new();
    let serving = stop.clone();
    let holding = Arc::new(OnTheRuntime { db: leader });
    drop(runtime.spawn(async move {
        peers
            .serve(
                serving,
                LEADER,
                Arc::new(Deciding::holding(settled())),
                holding,
            )
            .await
    }));

    let refused = collect(&authority, address, 1, 64)
        .expect_err("a peer nobody subscribed may not take the log");
    stop.cancel();
    assert!(
        matches!(refused, Error::Unsubscribed),
        "the runtime door answered otherwise: {refused}"
    );
}

/// ADR-0106 D5: a follower holding a stream is SENT the leader's next
/// commit — no second ask, no clock — and hears heartbeats while it waits.
#[test]
fn a_held_stream_is_sent_a_commit_the_moment_it_lands() {
    let authority = Authority::new();
    let leader = granting(" REPLICATES STORE");
    let peers = crate::link::tests::bind_with(
        "127.0.0.1:0",
        authority.issue(LEADER, Purpose::Peer),
        &authority.der(),
    )
    .expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the door");
    let stop = tokio_util::sync::CancellationToken::new();
    let serving = stop.clone();
    let holding = Arc::new(OnTheRuntime {
        db: Arc::clone(&leader),
    });
    drop(runtime.spawn(async move {
        peers
            .serve(
                serving,
                LEADER,
                Arc::new(Deciding::holding(settled())),
                holding,
            )
            .await
    }));

    let mut following = super::super::Following::open(
        (LEADER, address),
        &authority.keys(THERE, Purpose::Peer),
        &hello(THERE),
        Duration::from_secs(5),
    )
    .expect("a held stream");
    // Every log the leader holds, as a follower of the whole store asks.
    let homes = super::super::logs_to_collect(leader.store()).expect("the leader's logs");
    let ask = |from: &[Sequence]| super::super::StreamAsk {
        asks: homes
            .iter()
            .zip(from)
            .map(|(home, from)| Collect {
                home: *home,
                from: *from,
                limit: 1024,
            })
            .collect(),
    };
    // Everything the leader holds, first: one round with records, or, if
    // every log is empty, heartbeats only.
    let start = vec![Sequence::new(1); homes.len()];
    following.ask(&ask(&start)).expect("the first ask");
    let first = loop {
        let round = following.heard().expect("the first round");
        if !round.is_heartbeat() {
            break round;
        }
    };
    let held: Vec<Sequence> = first
        .answers
        .iter()
        .zip(&start)
        .map(|(answer, from)| {
            answer
                .records
                .last()
                .map_or(Sequence::new(from.get() - 1), |(at, _)| *at)
        })
        .collect();
    // Level now: the leader has nothing after `held` and must not answer
    // with records until it commits again.
    let next: Vec<Sequence> = held.iter().map(|at| Sequence::new(at.get() + 1)).collect();
    following
        .ask(&ask(&next))
        .expect("the ask from where the follower stands");
    let committing = Arc::clone(&leader);
    let committer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(350));
        let committed = std::time::Instant::now();
        committing
            .session()
            .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:2 = { name: 'grace' };")
            .expect("the leader commits");
        committed
    });
    let mut heartbeats = 0_u32;
    let waiting = std::time::Instant::now();
    let round = loop {
        let round = following.heard().expect("a frame from the leader");
        if round.is_heartbeat() {
            heartbeats += 1;
            assert!(
                waiting.elapsed() < Duration::from_secs(3),
                "the commit never reached the held stream ({heartbeats} heartbeats)"
            );
            continue;
        }
        break round;
    };
    let received = std::time::Instant::now();
    let committed = committer.join().expect("the committer");
    stop.cancel();
    assert!(
        heartbeats >= 2,
        "the leader said nothing while it waited ({heartbeats} heartbeats in 350 ms)"
    );
    // Every record sent starts where the follower stood in its log.
    for ((answer, from), home) in round.answers.iter().zip(&next).zip(&homes) {
        if let Some((at, _)) = answer.records.first() {
            assert_eq!(at, from, "{home:?} sent from the wrong place");
        }
    }
    assert!(!round.is_quiet(), "the round carried the commit");
    let waited = received.saturating_duration_since(committed);
    eprintln!("STREAM a commit reached the held stream {waited:?} after it was made");
    assert!(
        waited < Duration::from_millis(250),
        "the commit took {waited:?} to reach a held stream"
    );
}
