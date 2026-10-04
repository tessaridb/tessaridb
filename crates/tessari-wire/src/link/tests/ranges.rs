use super::*;

#[test]
fn a_range_ballot_is_judged_on_both_greetings_positions_for_that_range() {
    // G032 S3.2. The voter stands for shard 2 and its own log of it reaches
    // further than the candidate's: a ballot on shard 2 is refused as behind,
    // while the same candidate's store ballot -- level on the store -- and
    // its ballot on shard 3, which the voter never stood for, are granted.
    use crate::peer::Line;
    use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
    let shard = |n: u32| {
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            ShardId::new(n),
        )
    };
    let line = |n: u32, tail: u64| Line {
        range: shard(n),
        leading: Epoch::ZERO,
        tail: Sequence::new(tail),
        tail_leadership: Epoch::new(2),
    };
    let authority = Authority::new();
    let (peers, mut mine) = door(&authority);
    mine.line = Some(line(2, 40));
    let address = peers.address().expect("the door's address");
    let deciding = Deciding::holding(settled());
    let answering = std::thread::spawn(move || {
        (0..3)
            .map(|_| {
                peers.greet(
                    || Ok(mine),
                    &HERE,
                    &deciding,
                    &Placing(vec![shard(2), shard(3)]),
                )
            })
            .collect::<Vec<_>>()
    });
    let mut candidate = hello(THERE);
    candidate.line = Some(line(2, 3));
    let ask = |range: Reach| {
        let ballot = Round::opened(Epoch::new(1), THERE, 3).over(range).ballot();
        let (_, answered) = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &candidate,
            Ask::Ballot(&ballot),
        )
        .expect("the door is up");
        voted(&answered).expect("a door that was asked answers")
    };
    assert!(
        matches!(ask(shard(2)), Vote::Refused(Refused::LogBehind { tail, .. }) if tail == Sequence::new(40)),
        "behind on the range it asked for"
    );
    assert_eq!(
        ask(Reach::Store),
        Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        },
        "level on the store"
    );
    assert_eq!(
        ask(shard(3)),
        Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        },
        "the voter never stood for shard 3"
    );
    drop(answering.join().expect("the door's thread"));
}

/// A door with no log whose catalog places `THERE` on the ranges it holds.
struct Placing(Vec<tessari_types::Reach>);

impl Origin for Placing {
    fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
        NoLog.collected(follower, asked)
    }

    fn gathered(
        &self,
        asker: [u8; NODE_ID_LEN],
        asked: &crate::gathering::Gather,
    ) -> Result<crate::gathering::Page> {
        NoLog.gathered(asker, asked)
    }

    fn copied(
        &self,
        follower: [u8; NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        NoLog.copied(follower, write)
    }

    fn places(&self, candidate: [u8; NODE_ID_LEN], range: tessari_types::Reach) -> bool {
        candidate == THERE && self.0.contains(&range)
    }
}

/// A door whose catalog places `THERE` on one range, and whose store holds
/// that range's line to a position its greeting does not mention — the
/// former leader after a move, or a follower that collected the line.
struct Former(tessari_types::Reach, crate::grant::Reached);

impl Origin for Former {
    fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
        NoLog.collected(follower, asked)
    }

    fn gathered(
        &self,
        asker: [u8; NODE_ID_LEN],
        asked: &crate::gathering::Gather,
    ) -> Result<crate::gathering::Page> {
        NoLog.gathered(asker, asked)
    }

    fn copied(
        &self,
        follower: [u8; NODE_ID_LEN],
        write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        NoLog.copied(follower, write)
    }

    fn places(&self, candidate: [u8; NODE_ID_LEN], range: tessari_types::Reach) -> bool {
        candidate == THERE && range == self.0
    }

    fn reached_on(&self, range: tessari_types::Reach) -> Result<Option<crate::grant::Reached>> {
        Ok((range == self.0).then_some(self.1))
    }
}

#[test]
fn a_voter_holding_a_line_it_no_longer_leads_refuses_a_candidate_behind_it() {
    // Q-884. A move took the placement from this voter, so its greeting
    // names no line; its store still holds the line's log to 40, written
    // under leadership 2. A candidate at 3 must be refused as behind — the
    // greeting alone would read this voter as empty there and grant it,
    // and the candidate's leadership would continue the line from 3.
    use crate::peer::Line;
    use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
    let shard = Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        ShardId::new(2),
    );
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    assert!(mine.line.is_none(), "the voter greets with no line");
    let address = peers.address().expect("the door's address");
    let deciding = Deciding::holding(settled());
    let held = crate::grant::Reached {
        leadership: Epoch::new(2),
        tail: Sequence::new(40),
    };
    let answering = std::thread::spawn(move || {
        peers.greet(|| Ok(mine), &HERE, &deciding, &Former(shard, held))
    });
    let mut candidate = hello(THERE);
    candidate.line = Some(Line {
        range: shard,
        leading: Epoch::ZERO,
        tail: Sequence::new(3),
        tail_leadership: Epoch::new(2),
    });
    let ballot = Round::opened(Epoch::new(3), THERE, 3).over(shard).ballot();
    let (_, answered) = call_with(
        address,
        authority.issue(THERE, Purpose::Peer),
        &authority.der(),
        HERE,
        &candidate,
        Ask::Ballot(&ballot),
    )
    .expect("the door is up");
    assert!(
        matches!(
            voted(&answered),
            Some(Vote::Refused(Refused::LogBehind { tail, .. })) if tail == Sequence::new(40)
        ),
        "judged on the line the voter holds: {:?}",
        voted(&answered)
    );
    drop(answering.join().expect("the door's thread"));
}

#[test]
fn a_range_ballot_from_a_candidate_not_placed_on_it_is_refused() {
    // ADR-0098. Once this voter's catalog no longer places the candidate on
    // shard 3, the candidate cannot renew there; its store ballot and its
    // ballot on the shard it is placed on are judged as before.
    use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
    let shard = |n: u32| {
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            ShardId::new(n),
        )
    };
    let authority = Authority::new();
    let (peers, mine) = door(&authority);
    let address = peers.address().expect("the door's address");
    let deciding = Deciding::holding(settled());
    let placing = Placing(vec![shard(2)]);
    let answering = std::thread::spawn(move || {
        (0..3)
            .map(|_| peers.greet(|| Ok(mine), &HERE, &deciding, &placing))
            .collect::<Vec<_>>()
    });
    let candidate = hello(THERE);
    let ask = |range: Reach| {
        let ballot = Round::opened(Epoch::new(1), THERE, 3).over(range).ballot();
        let (_, answered) = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &candidate,
            Ask::Ballot(&ballot),
        )
        .expect("the door is up");
        voted(&answered).expect("a door that was asked answers")
    };
    assert_eq!(ask(shard(3)), Vote::Refused(Refused::NotPlaced));
    assert_eq!(
        ask(shard(2)),
        Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        },
        "placed on shard 2"
    );
    assert_eq!(
        ask(Reach::Store),
        Vote::Granted {
            hold: tessari_storage::LEASE_TTL
        },
        "the store is not placed"
    );
    drop(answering.join().expect("the door's thread"));
}
