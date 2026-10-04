use super::*;

#[test]
fn a_collection_frame_round_trips() {
    let asked = Collect {
        home: Reach::Database(NamespaceId::new(4), DatabaseId::new(9)),
        from: Sequence::new(7),
        limit: 64,
    };
    assert_eq!(Collect::decode(&asked.encode()).expect("an ask"), asked);
    // The range is the field a number means nothing without, so it
    // round-trips at every level rather than at the one the fixture happened
    // to pick.
    for home in [
        Reach::Store,
        Reach::Namespace(NamespaceId::new(1)),
        Reach::Database(NamespaceId::new(1), DatabaseId::new(2)),
    ] {
        let asked = Collect {
            home,
            from: Sequence::new(1),
            limit: 8,
        };
        assert_eq!(Collect::decode(&asked.encode()).expect("an ask").home, home);
    }

    let answer = Collected {
        log: LogId::unattributed(Reach::Store),
        previous: Epoch::new(3),
        records: vec![
            (Sequence::new(7), LogRecord::at(Epoch::new(4), Vec::new())),
            (Sequence::new(8), LogRecord::at(Epoch::new(4), Vec::new())),
        ],
        stopped_early: true,
        over: None,
        order: None,
        epoch: None,
    };
    let back = Collected::decode(&answer.encode()).expect("an answer");
    assert_eq!(back, answer);
    // `true` above and `false` here, because a flag that survived a round
    // trip in one state only would pass a test written with either.
    let full = Collected {
        stopped_early: false,
        ..answer.clone()
    };
    assert_eq!(
        Collected::decode(&full.encode()).expect("an answer"),
        full,
        "the flag travels in both states"
    );
    // A body from a leader with no budget carries no flag at all, and reads
    // as the thing such a leader always was: never stopped early.
    let mut older = full.encode();
    older.pop();
    assert_eq!(
        Collected::decode(&older).expect("an answer with no flag"),
        full,
        "a body with no flag reads as an answer that did not stop early"
    );
    // The leadership before the batch travels separately from the ones
    // inside it, and they differ here on purpose: a codec that carried one
    // of them twice would pass a test where they were equal.
    assert_eq!(back.previous, Epoch::new(3));
}

/// The wire is the first of S1.3's three carriers, and it carries the stamp
/// without knowing it exists.
///
/// That is the point rather than an accident of this test: the frame carries
/// a log record's encoded bytes, and the stamp lives inside a mutation's
/// value, so nothing in this crate had to change for a stamped record to
/// cross a link. The assertion below is what turns that from a claim into
/// evidence — it compares the bytes AND reads the stamp back out, because a
/// frame that dropped the flag bit would still produce equal-looking records
/// if only the payload were compared.
#[test]
fn a_stamped_record_survives_the_collection_frame() {
    use tessari_encoding::{CausalStamp, Mutation, RecordValue, StampedValue};
    use tessari_types::{RecordId, TableId};

    let mut stamp = CausalStamp::new();
    stamp.advance([1_u8; NODE_ID_LEN]);
    stamp.advance([1_u8; NODE_ID_LEN]);
    stamp.advance([2_u8; NODE_ID_LEN]);

    let record = LogRecord::at(
        Epoch::new(4),
        vec![Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            id: RecordId::from("contested"),
            shard: None,
            value: StampedValue::stamped(
                stamp.clone(),
                RecordValue::Present(b"from one of two masters".to_vec()),
            ),
        }],
    );
    let answer = Collected {
        log: LogId::unattributed(Reach::Store),
        previous: Epoch::new(3),
        records: vec![(Sequence::new(7), record)],
        stopped_early: false,
        over: None,
        order: None,
        epoch: None,
    };

    let encoded = answer.encode();
    let back = Collected::decode(&encoded).expect("an answer");
    assert_eq!(back, answer, "the frame did not carry the record unchanged");
    assert_eq!(
        back.encode(),
        encoded,
        "re-encoding what came off the wire did not reproduce the same bytes"
    );
    let carried = &back.records[0].1.mutations()[0].value;
    assert_eq!(carried.stamp(), &stamp, "the causal stamp did not survive");
    assert_eq!(carried.stamp().count(&[1_u8; NODE_ID_LEN]), 2);
    assert_eq!(carried.stamp().count(&[2_u8; NODE_ID_LEN]), 1);
}

#[test]
fn a_follower_collects_the_records_it_does_not_have() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 1);

    let collected =
        served(collect(&authority, address, 1, 64).expect("a peer that proved itself may collect"))
            .expect("a collection came back");
    door.join().expect("the door's thread");

    assert_eq!(
        collected
            .records
            .iter()
            .map(|(at, _)| at.get())
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the whole log, in order, from the position asked for"
    );
    // Nothing precedes the first position, and the answer says so with the
    // value a receiver would compute for itself.
    assert_eq!(collected.previous, Epoch::ZERO);
}

#[test]
fn a_collection_states_the_epoch_that_precedes_it() {
    let authority = Authority::new();
    // Three leaderships, one record each. Asking from 3 makes the answer
    // *2* while the leader's latest is *3* — which is the only arrangement
    // in which reading the epoch at the position and reading the store's
    // latest give different answers.
    let leader = logged(&[1, 2, 3]);
    let (address, door) = serving(&authority, &leader, 1);

    let collected =
        served(collect(&authority, address, 3, 64).expect("the door is up and answering"))
            .expect("a collection came back");
    door.join().expect("the door's thread");

    assert_eq!(
        collected.previous,
        Epoch::new(2),
        "the leadership at sequence 2, not the one at the tail"
    );
    assert_eq!(collected.records.len(), 1, "one record stands after 2");
}

#[test]
fn an_ask_tells_the_leader_what_the_follower_holds() {
    // ADR-0106 D6. A follower asks for the first position it does NOT hold,
    // once it has applied what it was sent — so an ask past what this
    // leader sent is the acknowledgement a write waiting for a majority
    // needs. An ask past what it was NOT sent vouches for nothing: a copy
    // of equal length that a deposed leadership finished asks the same way.
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 3);
    let log = store_log(&leader);
    let held = |at: u64| {
        leader.store().await_held(
            log,
            Sequence::new(at),
            &[THERE],
            1,
            std::time::Duration::ZERO,
        )
    };

    drop(served(
        collect(&authority, address, 3, 64).expect("a follower may collect"),
    ));
    assert!(
        held(2).is_empty(),
        "a follower's word counted for positions this leader never sent it"
    );

    drop(served(
        collect(&authority, address, 1, 64).expect("sent from the start"),
    ));
    drop(served(
        collect(&authority, address, 4, 64).expect("and asked past it"),
    ));
    door.join().expect("the door's thread");
    assert_eq!(
        held(3),
        vec![THERE],
        "the follower holds what it asked past"
    );
    assert!(
        held(4).is_empty(),
        "a position the follower asked for counted as held"
    );
}

#[test]
fn a_follower_that_asks_beyond_the_leaders_log_is_refused() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 2);

    // Level is not the same as beyond. A follower holding all three asks
    // from 4, the leader holds 3, and the honest answer is an EMPTY
    // collection — which is what makes the refusal below about the
    // position rather than about emptiness.
    let level = served(collect(&authority, address, 4, 64).expect("a level follower may ask"))
        .expect("a collection came back");
    assert!(level.records.is_empty(), "{:?}", level.records);
    assert_eq!(level.previous, Epoch::new(1), "the leadership at the tail");

    let refused = collect(&authority, address, 9, 64);
    door.join().expect("the door's thread");

    // And it crossed the wire as the refusal it was, not as a closed
    // socket: `from` is the position that was asked for.
    assert!(
        matches!(refused, Err(Error::Uncollectable { from: 9 })),
        "expected the position to be refused by name, got {refused:?}"
    );
}

#[test]
fn a_limit_bounds_one_collection_and_the_next_asks_from_where_it_stopped() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 2);

    let first = served(collect(&authority, address, 1, 2).expect("the first collection"))
        .expect("a collection came back");
    assert_eq!(
        first
            .records
            .iter()
            .map(|(at, _)| at.get())
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the bound the follower named, and not the whole log"
    );

    // No continuation state on the leader: the follower's cursor is the
    // position it reached, and the next ask is an ordinary one.
    let next = served(collect(&authority, address, 3, 2).expect("the second collection"))
        .expect("a collection came back");
    door.join().expect("the door's thread");
    assert_eq!(
        next.records
            .iter()
            .map(|(at, _)| at.get())
            .collect::<Vec<_>>(),
        vec![3],
        "the rest, starting where the first collection stopped"
    );
}

#[test]
fn a_collection_is_recorded_on_the_leader() {
    let authority = Authority::new();
    let leader = logged(&[1, 1, 1]);
    let (address, door) = serving(&authority, &leader, 1);

    assert!(
        leader
            .store()
            .follower_lag()
            .expect("a leader can be asked")
            .is_empty(),
        "nothing has collected yet"
    );

    drop(collect(&authority, address, 1, 2).expect("a collection"));
    door.join().expect("the door's thread");

    let lag = leader
        .store()
        .follower_lag()
        .expect("a leader can be asked");
    assert_eq!(lag.len(), 1, "{lag:?}");
    let seen = lag.first().expect("one follower");
    // By the id the HANDSHAKE proved, and at the position it was actually
    // handed — which is 2 and not 3, because the bound stopped the answer
    // short and the leader records what it gave rather than what it holds.
    assert_eq!(seen.node, THERE);
    assert_eq!(seen.sequence, Sequence::new(2));
    assert_eq!(seen.behind, 1, "one commit stands beyond what it was given");
}
