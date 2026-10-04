use super::*;

#[test]
fn a_subscriber_receives_its_namespace_and_not_the_one_beside_it() {
    /// One per level of the reach lattice, which is what bounds the chain.
    const ROUNDS: usize = 3;

    let authority = Authority::new();
    let leader = granting(" REPLICATES NAMESPACE prod");
    // One round per log the follower asks for, and it discovers the logs as
    // it goes: the store's own first, then the namespace that arrived in it.
    let (address, door) = declaring_for(&authority, &leader, ROUNDS);

    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let collector = collector(&mine, &said, address, 1024);

    // The chain, walked the way the node's own loop walks it: collect a
    // log, then re-derive the set, because the namespace whose log is worth
    // asking for only exists once the store's log has been applied.
    let mut reached = Sequence::ZERO;
    let mut asked: Vec<Reach> = Vec::new();
    for _ in 0..ROUNDS {
        let logs = logs_to_collect(follower.store()).expect("this node's own logs");
        // The next log this node has not asked for, or the store's own
        // again. Never a `break`: the door accepts exactly `ROUNDS`
        // connections, and stopping short leaves it waiting on one that
        // never arrives.
        let home = logs
            .into_iter()
            .find(|home| !asked.contains(home))
            .unwrap_or(Reach::Store);
        asked.push(home);
        // Each round asks a log this node has not asked for, so the first
        // position it does not hold there is the first one there is. Reading
        // its own tail would be the node's own loop and not this test's: the
        // raw feed is not something a test in a networked crate reaches
        // either, and `enforcement.rs` is right to say so.
        reached = collector
            .collect(follower.store(), home, Sequence::new(1))
            .expect("a subscribed peer collects");
    }
    door.join().expect("the door's thread");
    assert!(reached.get() >= 1, "the leader had a log to hand over");
    assert!(
        asked.contains(&Reach::Store),
        "the store's own log is where the namespace definition lives"
    );
    assert!(
        asked.iter().any(|home| matches!(home, Reach::Namespace(_))),
        "and the namespace's own log is where its records live: {asked:?}"
    );

    // Read back through a session rather than through the log, because what
    // the subscription is *for* is which records exist on the follower — and
    // a log comparison would pass on a build that transferred the bytes and
    // applied none of them.
    let mut session = follower.session();
    let held = session
        .run("USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users;")
        .expect("the subscribed namespace arrived");
    assert_eq!(held.len(), 3, "three statements, three outcomes");

    let missing = session
        .run("USE NAMESPACE other; USE DATABASE ledger; SELECT * FROM secrets;")
        .expect_err("the namespace beside the subscription must not have arrived");
    assert!(
        missing.to_string().contains("no namespace named \"other\""),
        "the namespace beside the subscription never arrived, and the \
             refusal names it: {missing}"
    );
    // And the follower recorded what it was served under, from the answer
    // rather than from anything typed here (G031, ADR-0081).
    assert!(
        matches!(follower.store().served(), Some(Reach::Namespace(_))),
        "the collect recorded its reach: {:?}",
        follower.store().served()
    );
}

/// A follower whose leader pruned past it copies the state, then follows
/// (ADR-0094 D3): the collect that used to be refused for ever is the
/// answer that means *copy me*, the copy carries the subscription and
/// nothing beside it, and the next collect continues from the positions the
/// copy stood the logs at.
#[test]
fn a_follower_below_a_pruned_log_copies_the_state_and_then_collects() {
    let authority = Authority::new();
    let leader = granting(" REPLICATES NAMESPACE prod");
    leader
        .session()
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; \
                 CREATE users:2 = { name: 'grace' }; CREATE users:3 = { name: 'edith' }; \
                 DELETE users:1;",
        )
        .expect("the leader moves on");
    leader
        .store()
        .set_log_retention(Some(Sequence::new(1)))
        .expect("a retention");
    leader.store().trim_logs().expect("the leader prunes");

    // Three connections: the refused collect, the copy, the collect after.
    let (address, door) = declaring_for(&authority, &leader, 3);
    let refused = collect(&authority, address, 1, 1024);
    assert!(
        matches!(refused, Err(Error::Uncollectable { .. })),
        "a position below the leader's log start asks for a copy: {refused:?}"
    );

    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let copied = crate::copy(&address.to_string(), &mine, LEADER, &said, follower.store())
        .expect("the copy lands");
    assert!(matches!(copied.over, Reach::Namespace(_)), "{copied:?}");

    let mut session = follower.session();
    let held = session
        .run("USE NAMESPACE prod; USE DATABASE orders; SELECT name FROM users;")
        .expect("the subscribed namespace arrived");
    assert_eq!(
        format!("{:?}", held.last()),
        format!(
            "{:?}",
            leader
                .session()
                .run("USE NAMESPACE prod; USE DATABASE orders; SELECT name FROM users;")
                .expect("the leader answers")
                .last()
        ),
        "the follower answers what its leader answers"
    );
    assert!(
        session
            .run("USE NAMESPACE other; USE DATABASE ledger; SELECT * FROM secrets;")
            .is_err(),
        "a copy carries the subscription and nothing beside it"
    );

    // The collect after the copy starts one past where the copy stood the
    // namespace's log, and is answered rather than refused.
    let (log, at) = *copied
        .positions
        .iter()
        .find(|(log, _)| log.home != Reach::Store)
        .expect("a log below the store's was copied");
    let collector = collector(&mine, &said, address, 1024);
    let reached = collector
        .collect(
            follower.store(),
            log.home,
            Sequence::new(at.get().saturating_add(1)),
        )
        .expect("the follower follows on from the copy");
    assert_eq!(reached, at, "nothing was written after the copy");
    door.join().expect("the door's thread");
}

#[test]
fn a_record_read_out_of_one_log_is_applied_into_that_same_log() {
    // The defect this slice closes, asserted by POSITION and not by
    // presence: a follower used to read the leader's namespace log and file
    // every record in its own store log, so the records arrived and the
    // sequences counted in a counter they never came from.
    const ROUNDS: usize = 2;

    let authority = Authority::new();
    let leader = granting(" REPLICATES NAMESPACE prod");
    let (address, door) = declaring_for(&authority, &leader, ROUNDS);

    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let collector = collector(&mine, &said, address, 1024);

    // The log the subscribed namespace's records actually live in. It is
    // the DATABASE's and not the namespace's: a record homes at the join of
    // the reaches its mutations carried to, and a `CREATE` inside one
    // database joins to that database. The leader holds no namespace-level
    // log at all, which is worth knowing before writing an assertion about
    // one — `leader.store().logs()` answers it and compiles nothing.
    let inside = leader
        .store()
        .logs()
        .expect("the leader's own logs")
        .into_iter()
        .find(|log| matches!(log.home, Reach::Database(namespace, _) if namespace.get() == 1))
        .expect("the fixture writes records inside prod's database");
    for home in [Reach::Store, inside.home] {
        collector
            .collect(follower.store(), home, Sequence::new(1))
            .expect("a subscribed peer collects");
    }
    door.join().expect("the door's thread");

    // Positions rather than a count, because the defect being closed put the
    // right records in the wrong counter: a follower that folded them into
    // its store log would hold every record and no position in this one.
    let at = |db: &Db, log| -> Vec<Sequence> {
        db.store()
            .log_records(log, Sequence::ZERO, 64)
            .expect("a log reads back")
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect()
    };
    let theirs = at(&leader, inside);
    assert!(
        !theirs.is_empty(),
        "the fixture must put records in that log for this to assert anything"
    );
    assert_eq!(
        at(&follower, inside),
        theirs,
        "the records were read out of that log and must count in the \
             follower's copy of it, at the same positions"
    );
    assert!(
        follower
            .store()
            .logs()
            .expect("the follower's logs")
            .contains(&inside),
        "and the log must exist on the follower rather than its records \
             having been folded into the store's"
    );
}

#[test]
fn a_log_no_subscription_reaches_is_refused_rather_than_served() {
    let authority = Authority::new();
    let leader = granting(" REPLICATES NAMESPACE prod");
    let (address, door) = declaring(&authority, &leader);

    let beside = logs_to_collect(leader.store())
        .expect("the leader's own logs")
        .into_iter()
        .rfind(|home| matches!(home, Reach::Namespace(_)))
        .expect("the fixture declares two namespaces");
    let refused = collect_from(&authority, address, beside, 1, 64)
        .expect_err("a namespace this peer is not subscribed to");
    door.join().expect("the door's thread");

    // The same refusal a peer nobody subscribed gets, and deliberately so:
    // the repair is the same `REPLICATES` clause, and a fourth frame would
    // send an operator to it by a different sentence.
    assert!(
        matches!(refused, Error::Unsubscribed),
        "a log outside the grant is a refusal and not an empty answer: {refused}"
    );
    let said = refused.to_string();
    assert!(
        said.contains("reaches the log it asked for"),
        "and the sentence is true of a partial subscription too: {said}"
    );
}

#[test]
fn a_bounded_read_this_node_could_not_answer_becomes_answerable_once_it_collects() {
    // What the whole wave is for, stated as the one observable that changed.
    // Before this build a follower's `current_as_of` could only ever be
    // `None` — there was no code path by which a node that may not write
    // became level with anything — so every bounded read on every follower
    // was refused, and *this node is too far behind* could not be told apart
    // from *this node has never heard from anybody*.
    let authority = Authority::new();
    let leader = granting(" REPLICATES STORE");
    let (address, door) = declaring(&authority, &leader);

    let follower = Db::in_memory().expect("an in-memory store");
    // First, and it is not interchangeable with the lines that follow: a
    // node that may not write may not define anything either, so the role
    // has to be taken before there is a schema — which here there never is,
    // because the schema arrives by collection.
    follower
        .session()
        .run("DEFINE NODE ROLES serving;")
        .expect("a node may say what it is for");
    assert_eq!(
        follower
            .store()
            .current_as_of()
            .expect("a store can say how old its copy is"),
        None,
        "a node that has collected nothing has no known age, which is \
             outside every bound rather than inside the ones nobody measured"
    );

    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let reached = collector(&mine, &said, address, 1024)
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("a subscribed peer collects");
    door.join().expect("the door's thread");
    assert!(reached.get() > 1, "the leader had a log to hand over");

    // A short answer is the one moment a follower can observe that its copy
    // was current: the peer served fewer than the limit, so it had no more.
    let age = follower
        .store()
        .current_as_of()
        .expect("a store can say how old its copy is")
        .expect("a follower that collected to the end knows how old it is");
    assert!(
        age < std::time::Duration::from_secs(tessari_constants::STALENESS_FLOOR_SECONDS),
        "a copy that has just become level is inside the tightest bound the \
             API admits, and this one reads {age:?}"
    );

    // And the read that could not be answered before is answered now, by
    // this node, without leaving it.
    let answered = follower
        .session()
        .run(&format!(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users STALENESS {}s;",
            tessari_constants::STALENESS_FLOOR_SECONDS
        ))
        .expect("a copy inside the bound answers the read here");
    assert_eq!(answered.len(), 3, "three statements, three outcomes");
}

#[test]
fn a_follower_that_collects_applies_what_it_was_given() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 1);

    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let reached = collector(&mine, &said, address, 64)
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("a collection applies");
    door.join().expect("the door's thread");

    assert_eq!(reached, Sequence::new(3));
    assert_eq!(
        follower
            .store()
            .log_records(store_log(&leader), Sequence::new(1), 64)
            .expect("the log can be read")
            .len(),
        3,
        "the follower holds what it was given"
    );
}

#[test]
fn a_batch_is_applied_against_the_record_before_each_one() {
    let authority = Authority::new();
    // Three leaderships in one batch. The answer states only what precedes
    // the FIRST record; if every record were applied against that same
    // epoch, the second would claim `Epoch::ZERO` stands at position 1 while
    // the follower has just written epoch 1 there, and the store would
    // refuse it as a divergence.
    let leader = logged(&[1, 2, 3]);
    let (address, door) = serving(&authority, &leader, 1);

    let follower = Db::in_memory().expect("an in-memory store");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let reached = collector(&mine, &said, address, 64)
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("a batch of three leaderships applies");
    door.join().expect("the door's thread");

    assert_eq!(reached, Sequence::new(3));
}

#[test]
fn a_collection_whose_predecessor_disagrees_is_refused() {
    let authority = Authority::new();
    // The two histories agree on how FAR they go and disagree on who wrote
    // it. Nothing about the offered record says so — the check is the
    // predecessor, which is why the frame carries one at all.
    let leader = logged(&[9, 9, 9]);
    let (address, door) = serving(&authority, &leader, 1);

    // Standing in the LEADER's log, two records in. That is where a
    // follower's copy of it lives, and it is the only place the histories
    // can disagree at all: two writers' logs are two counters, so a record
    // of one never lands at a position of the other.
    let follower = logged_as(store_log(&leader).writer, &[1, 1]);
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let refused = collector(&mine, &said, address, 64).collect(
        follower.store(),
        Reach::Store,
        Sequence::new(3),
    );
    door.join().expect("the door's thread");

    // Named as a fork, not as any refusal: retrying meets the same record
    // on every pass, so the round takes it to the copy that repairs one
    // (ADR-0107, Q-879 H2).
    assert!(
        matches!(&refused, Err(Error::Forked { .. })),
        "expected the store's own refusal, named as a fork, got {refused:?}"
    );
    let message = match refused {
        Err(Error::Forked { message }) => message,
        _ => String::new(),
    };
    // The store's own words, carried through: a reworded divergence gives
    // an operator two accounts of one event.
    assert!(
        message.contains("epoch 1") && message.contains("epoch 9"),
        "{message}"
    );
    assert_eq!(
        follower
            .store()
            .log_records(store_log(&leader), Sequence::new(1), 64)
            .expect("the log can be read")
            .len(),
        2,
        "and nothing was appended"
    );
}

#[test]
fn a_level_answer_whose_predecessor_disagrees_is_refused() {
    let authority = Authority::new();
    // The fork the paused-leader test left standing: both copies are three
    // records long and a different leadership wrote the last one. The
    // leader has nothing past the follower's tail, so the answer carries no
    // record for a per-record check to run on — and the follower read
    // *you are level* off it while holding a record the line never had.
    // Raft's AppendEntries checks the previous entry with no entries too.
    let leader = logged(&[9, 9, 9]);
    let (address, door) = serving(&authority, &leader, 1);

    let follower = logged_as(store_log(&leader).writer, &[1, 1, 1]);
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let refused = collector(&mine, &said, address, 64).collect(
        follower.store(),
        Reach::Store,
        Sequence::new(4),
    );
    door.join().expect("the door's thread");

    assert!(
        matches!(&refused, Err(Error::Forked { .. })),
        "a follower holding a record the line never had read itself level: {refused:?}"
    );
}

#[test]
fn a_short_answer_tells_the_follower_how_old_its_copy_is() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 2);

    let follower = Db::in_memory().expect("an in-memory store");
    // A node that may not write: the half of `current_as_of` this wave is
    // about. A writable node answers zero by identity and would prove
    // nothing here.
    follower.hold_lease(Duration::ZERO);
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);

    // Bound of two against a log of three: the answer fills the bound, so
    // the follower asked and did not arrive.
    collector(&mine, &said, address, 2)
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("the first collection");
    assert_eq!(
        follower
            .store()
            .current_as_of()
            .expect("a store can be asked"),
        None,
        "a full answer is contact, not arrival"
    );

    // The rest arrives inside the bound, so the peer had no more.
    collector(&mine, &said, address, 2)
        .collect(follower.store(), Reach::Store, Sequence::new(3))
        .expect("the second collection");
    door.join().expect("the door's thread");
    assert!(
        follower
            .store()
            .current_as_of()
            .expect("a store can be asked")
            .is_some(),
        "a short answer is the peer saying it had no more"
    );
}
