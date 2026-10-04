use super::*;

#[test]
fn a_collection_stops_at_the_byte_budget_and_says_that_it_did() {
    let db = logged(&[1, 1, 1, 1, 1, 1]);
    // Room for two records and not the third, against a follower asking for
    // the whole log — which is the ask nothing caps.
    let serving = Serving::within(db.store(), &Everything, one_record() * 2);
    let (records, stopped_early) = serving
        .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
        .expect("a store-reach read of its own log");

    assert_eq!(
        records.len(),
        2,
        "the budget bounds the answer, not the ask"
    );
    assert!(
        stopped_early,
        "an answer the budget cut short says so, because the receiver cannot tell"
    );
}

#[test]
fn the_answer_after_a_budgeted_one_resumes_where_it_stopped() {
    let db = logged(&[1, 1, 1, 1, 1, 1]);
    let serving = Serving::within(db.store(), &Everything, one_record() * 2);
    let (first, _) = serving
        .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
        .expect("a store-reach read of its own log");
    let next = Sequence::new(
        first
            .last()
            .expect("the first answer carried records")
            .0
            .get()
            .saturating_add(1),
    );

    // There is no continuation state on the leader — the cursor is a value
    // the collector holds — so resuming is the same call from a later
    // position, which is what a follower actually does.
    let roomy = Serving::within(db.store(), &Everything, one_record() * 64);
    let (second, stopped_early) = roomy
        .fill(Reach::Store, store_log(&db), next, usize::MAX)
        .expect("a store-reach read of its own log");

    assert_eq!(second.len(), 4, "the rest of the log, and none of it twice");
    assert_eq!(second.first().expect("records").0, Sequence::new(3));
    assert!(
        !stopped_early,
        "the second answer reached the end of the log and did not stop early"
    );
}

#[test]
fn a_record_that_alone_exceeds_the_budget_is_still_carried() {
    let db = logged(&[1, 1, 1]);
    let serving = Serving::within(db.store(), &Everything, 0);
    let (records, stopped_early) = serving
        .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
        .expect("a store-reach read of its own log");

    // A budget that could answer nothing would leave a follower asking for
    // the same position forever, which is worse than the frame it costs.
    assert_eq!(records.len(), 1, "the first record is carried regardless");
    assert!(stopped_early);
}

#[test]
fn an_answer_that_reached_the_end_of_the_log_did_not_stop_early() {
    let db = logged(&[1, 1, 1]);
    let serving = Serving::within(db.store(), &Everything, one_record() * 64);
    let (records, stopped_early) = serving
        .fill(Reach::Store, store_log(&db), Sequence::new(1), usize::MAX)
        .expect("a store-reach read of its own log");

    assert_eq!(records.len(), 3);
    assert!(
        !stopped_early,
        "*you are level* and *the budget filled* are opposite instructions"
    );
}

#[test]
fn a_follower_whose_leader_stopped_on_the_budget_does_not_record_itself_current() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    // Room for one record against a log of three, so the leader stops on
    // bytes with the follower's own record count nowhere near full.
    let (address, door) = serving_within(&authority, &leader, 1, one_record());

    let follower = Db::in_memory().expect("an in-memory store");
    // A node that may not write, because a writable one answers zero by
    // identity and would prove nothing here.
    follower.hold_lease(Duration::ZERO);
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);

    // A generous count: the answer comes back short of it, which is the
    // reading that used to mean *the peer had no more* and now does not.
    collector(&mine, &said, address, 64)
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("the collection");
    door.join().expect("the door's thread");

    assert_eq!(
        follower
            .store()
            .current_as_of()
            .expect("a store can be asked"),
        None,
        "an answer the budget cut short is contact, not arrival — a follower \
             that read it as arrival would report a copy as current while it is \
             three records behind, and *current* is what a staleness bound reads"
    );
}

/// S3.2 — the refusal, and the only thing that lifts it.
///
/// The unit half of ADR-0078. A live run asserts the same two words reach an
/// operator through a real node's log; this asserts the decision itself, in
/// one process and without a cadence, so the message can be changed with a
/// test that fails in milliseconds rather than in ninety seconds.
///
/// The second half is the criterion's *destructive path exercised
/// separately*, and it is deliberately the SAME test: a refusal nothing can
/// lift is an outage, and a lift asserted apart from the refusal would pass
/// on a build where the two conditions had drifted apart.
#[test]
fn a_node_holding_a_tenancy_of_its_own_is_refused_until_it_removes_it() {
    let authority = Authority::new();
    let leader = granting(" REPLICATES STORE");
    // One, and that is an assertion in itself: the refusal spends NO
    // connection, because it is taken before the dial. A door serving two
    // would hang on an accept that never happens.
    let (address, door) = declaring_for(&authority, &leader, 1);

    let follower = Db::in_memory().expect("an in-memory store");
    follower
        .session()
        .run("DEFINE NAMESPACE research;")
        .expect("a node that holds something of its own");
    let mine = authority.keys(THERE, Purpose::Peer);
    let said = hello(THERE);
    let collector = collector(&mine, &said, address, 64);

    let refused = collector
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect_err("a node that holds a tenancy of its own may not collect");
    let words = refused.to_string();
    assert!(
        words.contains("research"),
        "the refusal names what would be reinterpreted, or an operator \
             cannot act on it: {words}"
    );
    assert!(
        words.contains("DROP NAMESPACE"),
        "and names the statement that lifts it: {words}"
    );

    // Refused means nothing arrived, not that the failure was reported after
    // the fact — which is the whole difference between this and a warning.
    // Read from the catalog rather than through `USE NAMESPACE`, which sets
    // the session's context without asking whether the namespace is there.
    assert_eq!(
        declared(&follower),
        vec!["research".to_owned()],
        "the cluster's namespaces must not have been applied"
    );

    // The destruction, stated by being performed. Nothing authorises it on
    // the joiner's behalf and nothing outlives it.
    follower
        .session()
        .run("DROP NAMESPACE research;")
        .expect("the operator removes their own tenancy");
    collector
        .collect(follower.store(), Reach::Store, Sequence::new(1))
        .expect("a node with no tenancy of its own has nothing to reinterpret");
    door.join().expect("the door's thread");

    assert_eq!(
        declared(&follower),
        vec!["prod".to_owned(), "other".to_owned()],
        "and then the cluster's namespaces arrive, under the ids the \
             cluster gave them"
    );
}
