use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::Lease;
use tessari_types::{Epoch, NamespaceId, Reach, Sequence};
use tokio_util::sync::CancellationToken;

use tessari_storage::{FailoverStamp, ReplicaDefinition};

use super::{
    Collecting, Published, Renewing, Seed, bootstrap_from, campaign_line, campaigns_for, due_in,
    election_timeout, every, heard_a_leader, heard_a_leader_on, heard_a_newer_policy,
    leader_of_range, names_a_peer, preferred_to_yield_to, released, stands, stands_for,
    stands_for_the_store, upstream, voters,
};
use crate::campaign::Stood;
use crate::directory::Directory;
use crate::grant::Leadership;
use crate::peer::Hello;

const NODE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];
/// The log every single-log fixture here counts in.
const STORE: Reach = Reach::Store;
const ANOTHER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];

/// A serving peer one second behind.
fn said() -> Hello {
    Hello {
        node: NODE,
        build: NodeVersion {
            major: 0,
            minor: 1,
            patch: 1,
        },
        epoch: Epoch::new(7),
        roles: Roles::SERVING,
        tail: Sequence::new(4096),
        tail_leadership: Epoch::new(7),
        current_as_of: Some(Duration::from_secs(1)),
        policy: None,
        line: None,
    }
}

/// A greeting from a node that may write right now.
///
/// `current_as_of` answering `Some(0)` is exactly what a node says when its
/// EFFECTIVE roles carry `writable`: it is the origin of what it holds, so
/// there is nothing for it to be stale relative to.
fn writing(epoch: Epoch) -> Hello {
    Hello {
        epoch,
        current_as_of: Some(Duration::ZERO),
        ..said()
    }
}

/// A greeting from a node that holds somebody else's writes.
fn following() -> Hello {
    said()
}

/// A directory holding one greeting per endpoint, all heard just now.
fn greeted(rows: &[(&str, Hello)]) -> Directory {
    let mut directory = Directory::new();
    let now = Instant::now();
    for (endpoint, said) in rows {
        directory.heard(endpoint, *said, now);
    }
    directory
}

/// A declared peer row naming a node at an address, writable and
/// coordinating — the shape every member of a cluster that can fail over
/// carries for every other member.
fn named(name: &str, endpoint: &str, node: [u8; NODE_ID_LEN]) -> ReplicaDefinition {
    ReplicaDefinition {
        name: name.to_owned(),
        endpoint: endpoint.to_owned(),
        roles: Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
        node: Some(node),
        ..peer(Roles::WRITABLE, Some(node))
    }
}

/// A declared peer row, as an operator would have written it.
fn peer(roles: Roles, node: Option<[u8; NODE_ID_LEN]>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: "leader".to_owned(),
        endpoint: "10.0.0.2:9000".to_owned(),
        roles,
        node,
        // Not read by `upstream` and set anyway: what the peer grants *this*
        // node lives on that peer's own catalog, not on this node's copy of
        // the row, and a value here that mattered would mean the follower
        // was deciding its own subscription.
        replicates: None,
        leads: None,
        clients: None,
        http: None,
        fingerprint: None,
        join: None,
        releasing: false,
        preferred: false,
        region: None,
    }
}

#[test]
fn a_node_that_may_write_collects_from_nobody() {
    // It is the origin of what it holds, which is exactly what
    // `current_as_of` says when it answers zero. Collecting into it would
    // apply a peer's records beside its own — the divergence the epoch chain
    // refuses at apply time, prevented here at the timer instead.
    let declared = [peer(Roles::WRITABLE, Some(NODE))];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(upstream(Roles::ALONE, &declared, &heard), None);
    assert_eq!(upstream(Roles::WRITABLE, &declared, &heard), None);
    // And the same node with the role taken away follows the same peer, so
    // the rule is the role and not something about the peer.
    assert_eq!(
        upstream(Roles::SERVING, &declared, &heard),
        Some((NODE, "10.0.0.2:9000".to_owned()))
    );
}

/// A seed, as an operator would have written it on the command line.
fn seed(node: [u8; NODE_ID_LEN], endpoint: &str) -> Seed {
    Seed {
        node,
        endpoint: endpoint.to_owned(),
    }
}

#[test]
fn a_joining_node_collects_from_the_seed_that_says_it_may_write_now() {
    // The one round that exists to break a circle: the membership lives in
    // the catalog, the catalog arrives by collecting from a member, and a
    // node that has just been told to join holds neither.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(
        bootstrap_from(Roles::SERVING, &seeds, &heard),
        Some((NODE, "10.0.0.2:9000".to_owned()))
    );
}

#[test]
fn a_node_that_may_write_does_not_collect_from_a_seed_either() {
    // The same rule `upstream` applies, and stated separately because the
    // two functions are siblings rather than one with a flag: a rule that
    // held in one of them and not the other would be a node that ignores
    // its peers and follows a command-line address.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(bootstrap_from(Roles::ALONE, &seeds, &heard), None);
    assert_eq!(bootstrap_from(Roles::WRITABLE, &seeds, &heard), None);
}

#[test]
fn a_seed_that_has_not_been_greeted_is_not_collected_from() {
    // Absence of a greeting is not evidence that a seed may write, and the
    // consequence here is sharper than it is for a declared peer: this node
    // holds nothing at all, so the first thing it collects is the whole of
    // what it will believe.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    assert_eq!(
        bootstrap_from(Roles::SERVING, &seeds, &Directory::new()),
        None
    );
}

#[test]
fn a_seed_that_is_a_follower_is_not_collected_from() {
    // A seed is an address an operator wrote down, and which node happens to
    // be leading is not a fact an operator can write down — so a seed
    // pointing at a node that may not write is the ordinary case, not a
    // misconfiguration. The joiner waits rather than pulling from a copy.
    let seeds = [seed(NODE, "10.0.0.2:9000")];
    let heard = greeted(&[("10.0.0.2:9000", following())]);
    assert_eq!(bootstrap_from(Roles::SERVING, &seeds, &heard), None);
}

#[test]
fn among_seeds_the_newer_leadership_is_collected_from() {
    // The same tie `upstream` breaks and for the same reason: a leader
    // demoted a moment ago and its successor can both be in this directory,
    // because a greeting is as fresh as the last awareness round.
    let seeds = [seed(NODE, "10.0.0.2:9000"), seed(ANOTHER, "10.0.0.3:9000")];
    let heard = greeted(&[
        ("10.0.0.2:9000", writing(Epoch::new(7))),
        ("10.0.0.3:9000", writing(Epoch::new(8))),
    ]);
    assert_eq!(
        bootstrap_from(Roles::SERVING, &seeds, &heard),
        Some((ANOTHER, "10.0.0.3:9000".to_owned()))
    );
}

#[test]
fn a_follower_with_no_writable_peer_collects_from_nobody() {
    assert_eq!(upstream(Roles::SERVING, &[], &Directory::new()), None);
}

#[test]
fn a_writable_peer_nobody_has_identified_cannot_be_collected_from() {
    // A peer connection demands a certificate valid for a name derived from
    // the peer's id, so an endpoint whose id nobody knows cannot be dialled
    // at all — the same wall the seed address runs into. `None` rather than
    // a half-formed attempt.
    let declared = [peer(Roles::WRITABLE, None)];
    let heard = greeted(&[("10.0.0.2:9000", writing(Epoch::new(7)))]);
    assert_eq!(upstream(Roles::SERVING, &declared, &heard), None);
}

#[test]
fn a_follower_collects_from_the_peer_that_says_it_may_write_now() {
    // ADR-0065, and the configuration that forced it: ADR-0063 and ADR-0064
    // together make *every coordinating node also declared writable* the
    // only shape in which a failover produces a writer, so two writable ROWS
    // is the normal cluster rather than a misconfiguration. The row says who
    // may be followed; the greeting says which of them is the origin now.
    let declared = [
        named("one", "10.0.0.2:9000", [1; NODE_ID_LEN]),
        named("two", "10.0.0.3:9000", [2; NODE_ID_LEN]),
    ];
    let heard = greeted(&[
        ("10.0.0.2:9000", following()),
        ("10.0.0.3:9000", writing(Epoch::new(4))),
    ]);
    assert_eq!(
        upstream(Roles::SERVING, &declared, &heard),
        Some(([2; NODE_ID_LEN], "10.0.0.3:9000".to_owned())),
        "both rows are declared writable; only one of them said it may write"
    );
}

#[test]
fn the_newer_leadership_wins_a_directory_holding_both() {
    // Not hypothetical. A greeting is as fresh as the last awareness round
    // and no fresher, so a leader demoted a moment ago and its successor are
    // both in this directory saying they may write. ADR-0059's ordering
    // settles it, which is the same rule a voter applies to a ballot.
    let declared = [
        named("old", "10.0.0.2:9000", [1; NODE_ID_LEN]),
        named("new", "10.0.0.3:9000", [2; NODE_ID_LEN]),
    ];
    let heard = greeted(&[
        ("10.0.0.2:9000", writing(Epoch::new(4))),
        ("10.0.0.3:9000", writing(Epoch::new(5))),
    ]);
    assert_eq!(
        upstream(Roles::SERVING, &declared, &heard),
        Some(([2; NODE_ID_LEN], "10.0.0.3:9000".to_owned()))
    );
}

#[test]
fn a_peer_this_node_has_never_greeted_is_not_followed() {
    // Absence of a greeting is not evidence that a peer may write. A cold
    // node collects from nobody until its first awareness round lands —
    // one interval of not collecting, against pulling records from whichever
    // address happened to be declared first.
    let declared = [named("one", "10.0.0.2:9000", [1; NODE_ID_LEN])];
    assert_eq!(upstream(Roles::SERVING, &declared, &Directory::new()), None);
}

/// The row a node acquires about ITSELF the moment replication works.
///
/// W382, against three processes. `DEFINE REPLICA` is a catalog write and
/// therefore a log record, so a follower that applies a leader's store log
/// receives the leader's membership — which names the follower. Counting it
/// makes a cluster of three demand three grants and leaves the third
/// uncastable, because the only node that would cast it is the candidate,
/// whose own door refuses the connection. Every round then fails and the
/// epoch climbs for ever with nothing in an error state.
#[test]
fn a_row_naming_this_node_is_not_one_of_its_own_voters() {
    let mine = Roles::SERVING.and(Roles::COORDINATING);
    let itself = peer(mine, Some(NODE));
    let other = named("two", "10.0.0.3:9000", ANOTHER);
    assert_eq!(
        voters(mine, std::slice::from_ref(&itself), &NODE),
        None,
        "a node stood a round against nobody but itself"
    );
    assert_eq!(
        voters(mine, &[itself, other], &NODE),
        Some(vec![(ANOTHER, "10.0.0.3:9000".to_owned())]),
        "the membership a round is judged against counted this node twice"
    );
}

#[test]
fn a_node_the_operator_did_not_make_coordinating_stands_for_nothing() {
    // ADR-0063. The deciding set is the set a leader is drawn from, and
    // `COORDINATING` is the role that names it. §6.1 still keeps the two
    // halves apart — the DESIRED role is a catalog record the operator
    // writes, the EFFECTIVE role is a lease the cluster grants — so a node
    // outside the deciding set promoting itself because a leader went quiet
    // would be taking the decision the catalog exists to hold.
    let coordinating = peer(Roles::SERVING.and(Roles::COORDINATING), Some(NODE));
    assert_eq!(
        voters(
            Roles::SERVING,
            std::slice::from_ref(&coordinating),
            &ANOTHER
        ),
        None
    );
    assert_eq!(
        voters(Roles::NONE, std::slice::from_ref(&coordinating), &ANOTHER),
        None
    );
    // Writable is no longer what lets a node stand, and this is the pair
    // that says so: `ALONE` is `SERVING|WRITABLE`, and it stands for
    // nothing; the same node declared `COORDINATING` and never writable
    // stands.
    assert_eq!(
        voters(Roles::ALONE, std::slice::from_ref(&coordinating), &ANOTHER),
        None
    );
    assert_eq!(
        voters(
            Roles::SERVING.and(Roles::COORDINATING),
            &[coordinating],
            &ANOTHER
        ),
        Some(vec![(NODE, "10.0.0.2:9000".to_owned())])
    );
}

#[test]
fn a_single_node_deployment_does_not_begin_campaigning() {
    // The guard rail ADR-0063 names as the risk a reader looks for first,
    // asserted rather than argued. `Roles::ALONE` is documented as *not
    // `COORDINATING`, because there is nothing to coordinate with*, so
    // widening who may stand makes an existing single-node store stand for
    // LESS than it did before — it never hands a store that has never needed
    // a lease a new way to stop accepting writes.
    assert!(!stands(Roles::ALONE));
    assert!(!stands(Roles::WRITABLE));
    assert!(stands(Roles::SERVING.and(Roles::COORDINATING)));
}

#[test]
fn a_node_with_nobody_to_coordinate_with_stands_for_nothing() {
    // A member of the deciding set that can reach no other member is not a
    // round of one, it is a node with nothing to decide. The mine here is
    // `COORDINATING` on purpose: with `ALONE` this test would pass on the
    // eligibility rule above and stop testing the membership rule it names.
    let mine = Roles::SERVING.and(Roles::COORDINATING);
    assert_eq!(voters(mine, &[], &ANOTHER), None);
    // A declared peer that does not coordinate is not a voter either — it
    // replicates, which is a different grant entirely.
    assert_eq!(
        voters(mine, &[peer(Roles::SERVING, Some(NODE))], &ANOTHER),
        None
    );
}

#[test]
fn a_voting_peer_nobody_has_identified_cannot_be_balloted() {
    // The wall `upstream` runs into, at the other cadence. A ballot travels
    // on a connection whose certificate must be valid for a name derived
    // from the peer's id, so a row naming where but not who cannot be asked
    // for anything — and dropping it from the membership matters twice over,
    // because a majority counted over members that cannot be asked is a
    // majority of a fiction.
    let named = peer(Roles::SERVING.and(Roles::COORDINATING), Some(NODE));
    let nameless = peer(Roles::SERVING.and(Roles::COORDINATING), None);
    let mine = Roles::SERVING.and(Roles::COORDINATING);
    assert_eq!(
        voters(mine, std::slice::from_ref(&nameless), &ANOTHER),
        None
    );
    assert_eq!(
        voters(mine, &[nameless, named], &ANOTHER),
        Some(vec![(NODE, "10.0.0.2:9000".to_owned())])
    );
}

#[test]
fn a_failed_collection_retries_from_the_same_position() {
    let mut collecting = Collecting::new();
    let seed = Sequence::new(5);
    let refused = collecting.once(STORE, seed, |_| Err::<Sequence, ()>(()));
    assert_eq!(
        refused,
        Err(()),
        "a failed pass answered as though it landed"
    );
    assert_eq!(collecting.reached(STORE), Some(Sequence::new(5)));

    let asked = RefCell::new(Vec::new());
    // A seed the retry must NOT take: the cursor exists now, so a pass that
    // read the seed again would be a pass that forgot where it failed.
    let reached = collecting.once(STORE, Sequence::new(99), |at| {
        asked.borrow_mut().push(at);
        Ok::<Sequence, ()>(Sequence::new(at.get() + 10))
    });
    assert_eq!(
        *asked.borrow(),
        vec![Sequence::new(5)],
        "the retry asked from somewhere other than where it failed"
    );
    assert_eq!(reached, Ok(Sequence::new(15)));
}

#[test]
fn a_collection_that_lands_advances_the_cursor() {
    let mut collecting = Collecting::new();
    assert_eq!(
        collecting.once(STORE, Sequence::new(1), |_| Ok::<Sequence, ()>(
            Sequence::new(9)
        )),
        Ok(Sequence::new(9))
    );
    assert_eq!(collecting.reached(STORE), Some(Sequence::new(9)));
}

/// The whole reason the cursor is a map: two logs, two counters.
#[test]
fn a_position_reached_in_one_log_is_not_a_position_in_another() {
    let prod = Reach::Namespace(NamespaceId::new(1));
    let mut collecting = Collecting::new();
    assert_eq!(collecting.reached(prod), None, "nothing collected anywhere");

    assert_eq!(
        collecting.once(STORE, Sequence::new(1), |_| Ok::<Sequence, ()>(
            Sequence::new(40)
        )),
        Ok(Sequence::new(40))
    );
    // The seed is what a namespace log with no cursor starts from, and the
    // store's log reaching 40 must not spend it: a single cursor would have
    // asked this log for position 40 and been answered a gap or a re-send
    // depending only on which log ran ahead.
    let asked = RefCell::new(Vec::new());
    assert_eq!(
        collecting.once(prod, Sequence::new(1), |at| {
            asked.borrow_mut().push(at);
            Ok::<Sequence, ()>(Sequence::new(3))
        }),
        Ok(Sequence::new(3))
    );
    assert_eq!(*asked.borrow(), vec![Sequence::new(1)]);
    assert_eq!(collecting.reached(STORE), Some(Sequence::new(40)));
    assert_eq!(collecting.reached(prod), Some(Sequence::new(3)));
}

/// The deadlock W256 measured against three processes, as a unit.
#[test]
fn a_lost_round_stands_higher_the_next_time_it_stands() {
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::ZERO,
        from: Instant::now(),
    });
    let stood_for = RefCell::new(Vec::new());
    let ask = |renewing: &mut Renewing, now: Instant| {
        renewing.once(NODE, now, |_, next| {
            stood_for.borrow_mut().push(next);
            Stood::Lost {
                granted: Epoch::ZERO,
            }
        });
    };

    let opened = Instant::now();
    ask(&mut renewing, opened);
    // Far enough past any stagger that the wait is not what is being tested.
    ask(&mut renewing, opened + Duration::from_secs(30));
    ask(&mut renewing, opened + Duration::from_secs(60));

    assert_eq!(
        *stood_for.borrow(),
        vec![Epoch::new(1), Epoch::new(2), Epoch::new(3)],
        "a candidate that lost stood for the same epoch again, against \
             voters that had already spent it — which is how three healthy \
             nodes elect nobody forever"
    );
}

/// The number is already in the answer the candidate is given.
#[test]
fn a_refusal_that_names_a_granted_epoch_is_learned_from() {
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::ZERO,
        from: Instant::now(),
    });
    let opened = Instant::now();
    renewing.once(NODE, opened, |_, _| Stood::Lost {
        granted: Epoch::new(50),
    });
    let stood_for = RefCell::new(None);
    renewing.once(NODE, opened + Duration::from_secs(30), |_, next| {
        *stood_for.borrow_mut() = Some(next);
        Stood::Lost {
            granted: Epoch::ZERO,
        }
    });
    assert_eq!(
        *stood_for.borrow(),
        Some(Epoch::new(51)),
        "a node that never led holds Epoch::ZERO however far the cluster \
             has got, so without adopting what the refusals report it would \
             climb one epoch per round to reach the conversation"
    );
}

/// A lost round is not retried on the same tick as everyone else's.
#[test]
fn a_candidate_that_just_lost_waits_before_standing_again() {
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::ZERO,
        from: Instant::now(),
    });
    let opened = Instant::now();
    renewing.once(NODE, opened, |_, _| Stood::Lost {
        granted: Epoch::ZERO,
    });
    let asked = RefCell::new(false);
    renewing.once(NODE, opened, |_, _| {
        *asked.borrow_mut() = true;
        Stood::Lost {
            granted: Epoch::ZERO,
        }
    });
    assert!(
        !*asked.borrow(),
        "a candidate stood again on the same instant it lost, which is how \
             three of them split every epoch as reliably as they split the first"
    );
}

/// And the wait is different per node, which is the whole of the property.
#[test]
fn two_candidates_do_not_come_back_at_the_same_instant() {
    let opened = Instant::now();
    let waited = |candidate: [u8; NODE_ID_LEN]| {
        let mut renewing = Renewing::holding(Leadership {
            length: tessari_storage::LEASE_TTL,
            epoch: Epoch::ZERO,
            from: opened,
        });
        renewing.once(candidate, opened, |_, _| Stood::Lost {
            granted: Epoch::ZERO,
        });
        // The first instant at which it will ask again, found by asking.
        (0..2000)
            .map(|millis| opened + Duration::from_millis(millis))
            .find(|at| {
                let asked = RefCell::new(false);
                renewing.once(candidate, *at, |_, _| {
                    *asked.borrow_mut() = true;
                    Stood::NotDue
                });
                *asked.borrow()
            })
            .expect("a stagger inside one round time")
    };
    assert_ne!(
        waited([1; NODE_ID_LEN]),
        waited([2; NODE_ID_LEN]),
        "two candidates that lost together came back together"
    );
}

/// The epoch is in the mix, so an unlucky pair does not collide forever.
#[test]
fn a_pair_that_collides_at_one_epoch_is_not_condemned_to_collide_at_every_one() {
    let offsets = |epoch: Epoch| {
        (0_u8..64)
            .map(|seed| Renewing::stagger([seed; NODE_ID_LEN], epoch))
            .collect::<Vec<_>>()
    };
    assert_ne!(
        offsets(Epoch::new(1)),
        offsets(Epoch::new(2)),
        "the offsets did not move with the epoch, so a pair whose ids fall \
             close together would collide at every epoch there is"
    );
}

#[test]
fn a_node_that_can_hear_a_leader_does_not_stand_against_it() {
    let leader = [9; NODE_ID_LEN];
    let declared = [named("leader", "10.0.0.1:9000", leader)];
    let heard = greeted(&[("10.0.0.1:9000", writing(Epoch::new(7)))]);
    assert!(
        heard_a_leader(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        "a follower stood against a leader it had just heard from, and its \
             own self-vote then refuses that leader's renewal for a whole lease"
    );
}

#[test]
fn a_greeting_older_than_the_lease_holds_nobody_back() {
    let leader = [9; NODE_ID_LEN];
    let declared = [named("leader", "10.0.0.1:9000", leader)];
    let mut heard = Directory::new();
    let long_ago = Instant::now();
    heard.heard("10.0.0.1:9000", writing(Epoch::new(7)), long_ago);
    assert!(
        !heard_a_leader(
            &declared,
            &heard,
            None,
            long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1),
            tessari_storage::LEASE_TTL
        ),
        "a greeting older than the leader's own lease cannot testify that \
             the leader still holds it, and a node that hears nothing has to \
             stand — that is what an election is for"
    );
}

#[test]
fn a_peer_that_is_not_the_origin_is_not_a_leader() {
    let peer = [9; NODE_ID_LEN];
    let declared = [named("peer", "10.0.0.1:9000", peer)];
    // `following()` carries a non-zero `current_as_of`: it holds somebody
    // else's writes, so it is not leading whatever its catalog row says.
    let heard = greeted(&[("10.0.0.1:9000", following())]);
    assert!(
        !heard_a_leader(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        "a follower was mistaken for a leader, so a cluster whose leader \
             died would never elect another"
    );
}

#[test]
fn a_grant_this_node_made_holds_it_back_when_the_directory_is_already_stale() {
    let leader = [9; NODE_ID_LEN];
    let declared = [named("leader", "10.0.0.1:9000", leader)];
    let mut heard = Directory::new();
    let long_ago = Instant::now();
    heard.heard("10.0.0.1:9000", writing(Epoch::new(7)), long_ago);
    // One whole lease after the greeting, which is the moment the directory
    // stops testifying — and half a lease after a renewal this node granted,
    // which is the whole point: a leader renews about every 300 ms and the
    // directory is refreshed every second.
    let now = long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1);
    let granted = now - tessari_storage::LEASE_TTL / 2;
    assert!(
        heard_a_leader(
            &declared,
            &heard,
            Some(granted),
            now,
            tessari_storage::LEASE_TTL
        ),
        "the directory had aged out but this node had granted that leader a \
             renewal half a lease ago — standing against a leader it just \
             acknowledged is exactly what the quiet-cluster gate exists to stop"
    );
}

#[test]
fn a_grant_older_than_the_lease_holds_nobody_back_either() {
    let declared = [named("leader", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let now = Instant::now();
    let granted = now - tessari_storage::LEASE_TTL - Duration::from_secs(1);
    assert!(
        !heard_a_leader(
            &declared,
            &Directory::new(),
            Some(granted),
            now,
            tessari_storage::LEASE_TTL
        ),
        "a grant older than the lease it granted cannot testify that the \
             holder still has it, and a node that hears nothing has to stand"
    );
}

/// A greeting from a node running the policy set at `(epoch, version)`.
fn running(epoch: u64, version: u64) -> Hello {
    Hello {
        policy: Some(FailoverStamp {
            epoch: Epoch::new(epoch),
            version,
        }),
        ..said()
    }
}

fn stamp(epoch: u64, version: u64) -> FailoverStamp {
    FailoverStamp {
        epoch: Epoch::new(epoch),
        version,
    }
}

#[test]
fn a_peer_running_a_newer_failover_policy_holds_this_node_back() {
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.1:9000", running(4, 0))]);
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            Some(stamp(3, 9)),
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        Some(stamp(4, 0)),
        "a candidate timing itself by a policy the cluster has replaced was              not held back, which is the disagreement the policy row exists to              remove arriving at the moment it decides an outcome"
    );
}

#[test]
fn an_equal_or_older_policy_holds_nobody_back() {
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let now = Instant::now();
    for (peer, mine, why) in [
        (
            running(3, 9),
            stamp(3, 9),
            "an equal pair is the ordinary state of an agreeing cluster and                  must never stop an election",
        ),
        (
            running(3, 8),
            stamp(3, 9),
            "a lower version is a peer that is behind, which is the ordinary                  state of a follower and not a reason to refuse",
        ),
        (
            running(2, 99),
            stamp(3, 0),
            "a superseded leadership does not win on version — this is the                  partitioned ex-leader reconnecting, and letting it silence a                  candidate would hand it the outcome it lost",
        ),
    ] {
        assert_eq!(
            heard_a_newer_policy(
                &declared,
                &greeted(&[("10.0.0.1:9000", peer)]),
                Some(mine),
                now,
                tessari_storage::LEASE_TTL
            ),
            None,
            "{why}"
        );
    }
}

#[test]
fn a_node_that_holds_no_policy_at_all_is_behind_one_that_does() {
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.1:9000", running(1, 0))]);
    // The first policy a cluster ever sets is the case this covers. Treating
    // *no policy* as unbeatable would make that first one the single policy
    // nothing could ever act on.
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        Some(stamp(1, 0))
    );
    // And the other direction: a peer that says nothing supersedes nothing,
    // so a build from before the field cannot silence the cluster it joins.
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &greeted(&[("10.0.0.1:9000", said())]),
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        None,
        "a greeting carrying no policy silenced a candidate, which would              make a rolling upgrade an outage"
    );
}

#[test]
fn a_greeting_older_than_the_lease_cannot_hold_a_candidate_back() {
    // This is what makes the gate incapable of deadlocking a cluster: the
    // refusal is bounded by audibility, so the node holding the newer policy
    // going away opens the gate rather than closing it forever.
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let mut heard = Directory::new();
    let long_ago = Instant::now();
    heard.heard("10.0.0.1:9000", running(4, 0), long_ago);
    let now = long_ago + tessari_storage::LEASE_TTL + Duration::from_secs(1);
    assert_eq!(
        heard_a_newer_policy(&declared, &heard, None, now, tessari_storage::LEASE_TTL),
        None,
        "a peer nobody has heard from in longer than a lease was still              silencing this node, so a cluster that lost the one node holding              the newer policy could never elect again"
    );
}

#[test]
fn a_policy_advertised_by_an_undeclared_address_is_not_a_member_speaking() {
    // The same rule `heard_a_leader` and `upstream` hold: a greeting from an
    // address this node's catalog does not declare is a stranger, and a
    // stranger that can silence a candidate is a denial of service with a
    // one-line implementation.
    let declared = [named("peer", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.9:9000", running(4, 0))]);
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        None
    );
}

#[test]
fn the_newest_policy_heard_is_the_one_reported() {
    // Reported rather than merely detected, so an operator reading the log
    // line knows WHICH policy this node is behind. With several peers at
    // several stamps, the answer has to be the newest or the report names a
    // policy that is itself superseded.
    let declared = [
        named("one", "10.0.0.1:9000", [9; NODE_ID_LEN]),
        named("two", "10.0.0.2:9000", [8; NODE_ID_LEN]),
        named("three", "10.0.0.3:9000", [7; NODE_ID_LEN]),
    ];
    let heard = greeted(&[
        ("10.0.0.1:9000", running(4, 1)),
        ("10.0.0.2:9000", running(5, 0)),
        ("10.0.0.3:9000", running(4, 9)),
    ]);
    assert_eq!(
        heard_a_newer_policy(
            &declared,
            &heard,
            Some(stamp(3, 0)),
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        Some(stamp(5, 0))
    );
}

#[test]
fn a_node_that_has_granted_nothing_is_answered_by_the_directory_exactly_as_before() {
    let declared = [named("leader", "10.0.0.1:9000", [9; NODE_ID_LEN])];
    let heard = greeted(&[("10.0.0.1:9000", writing(Epoch::new(7)))]);
    // `None` is *no such evidence*, never *no leader*. A follower outside
    // the deciding set grants nothing and must still be held back by a
    // greeting, or this change would make every non-voter campaign.
    assert!(
        heard_a_leader(
            &declared,
            &heard,
            None,
            Instant::now(),
            tessari_storage::LEASE_TTL
        ),
        "a node with no grant to read was not held back by a fresh greeting"
    );
}

#[test]
fn a_renewal_that_wins_nothing_keeps_the_lease_it_holds() {
    let held = Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from: Instant::now(),
    };
    let mut renewing = Renewing::holding(held);
    let standing = renewing.once(NODE, Instant::now(), |_, _| Stood::NotDue);
    assert_eq!(standing, held, "a round that won nothing changed the lease");
    assert_eq!(renewing.standing(), held);
}

#[test]
fn an_election_timeout_is_the_lease_plus_a_spread_that_differs_by_node() {
    // G053 SG2b, D4. Never shorter than the lease — a voter refuses all but
    // the incumbent until then — and never more than the spread past it;
    // and two nodes do not share one timeout at every epoch, which is the
    // property that keeps two followers from standing on the same tick.
    let spread = Duration::from_millis(tessari_constants::ELECTION_JITTER_MILLIS);
    let (one, other) = ([1_u8; NODE_ID_LEN], [2_u8; NODE_ID_LEN]);
    let mut differed = 0_u32;
    for epoch in 1..=64 {
        let epoch = Epoch::new(epoch);
        for node in [one, other] {
            let waited = election_timeout(node, epoch, tessari_storage::LEASE_TTL);
            assert!(waited >= tessari_storage::LEASE_TTL, "{waited:?}");
            assert!(
                waited < tessari_storage::LEASE_TTL.saturating_add(spread),
                "{waited:?}"
            );
        }
        if election_timeout(one, epoch, tessari_storage::LEASE_TTL)
            != election_timeout(other, epoch, tessari_storage::LEASE_TTL)
        {
            differed = differed.saturating_add(1);
        }
    }
    assert!(
        differed >= 60,
        "two nodes shared a timeout {} times in 64",
        64_u32.saturating_sub(differed)
    );
}

#[test]
fn a_renewal_re_asks_the_epoch_it_holds() {
    // G053 SG2b. A leader renews about every 300 ms, and an epoch per
    // renewal was a leadership record per renewal — three a second on an
    // idle cluster, eating the retained log and waking every stream. The
    // voter has always admitted the incumbent re-asking its own epoch.
    let from = Instant::now();
    let held = Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from,
    };
    let mut renewing = Renewing::holding(held);
    let stood_for = RefCell::new(Vec::new());
    let renewed = Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from: from + Duration::from_millis(300),
    };
    let standing = renewing.once(NODE, from, |_: Lease, next| {
        stood_for.borrow_mut().push(next);
        Stood::Won(renewed)
    });
    assert_eq!(*stood_for.borrow(), vec![Epoch::new(4)]);
    assert_eq!(standing, renewed, "a round that was won was not taken up");
}

#[test]
fn a_holder_told_of_a_higher_epoch_stands_above_it() {
    // The renewal keeps its epoch only while nothing says the cluster moved.
    // A refusal naming epoch 9 means a rival stood there; re-asking 4 would
    // be refused for ever, so the next stand is above what was heard.
    let from = Instant::now();
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::new(4),
        from,
    });
    renewing.once(NODE, from, |_, _| Stood::Lost {
        granted: Epoch::new(9),
    });
    let later = from + Duration::from_secs(5);
    let stood_for = RefCell::new(Vec::new());
    renewing.once(NODE, later, |_: Lease, next| {
        stood_for.borrow_mut().push(next);
        Stood::NotDue
    });
    assert_eq!(*stood_for.borrow(), vec![Epoch::new(10)]);
}

#[test]
fn a_node_that_never_led_stands_for_a_new_epoch() {
    let from = Instant::now();
    let mut renewing = Renewing::holding(Leadership {
        length: tessari_storage::LEASE_TTL,
        epoch: Epoch::ZERO,
        from,
    });
    let stood_for = RefCell::new(Vec::new());
    renewing.once(NODE, from, |_: Lease, next| {
        stood_for.borrow_mut().push(next);
        Stood::NotDue
    });
    assert_eq!(*stood_for.borrow(), vec![Epoch::new(1)]);
}

#[test]
fn a_delayed_cadence_runs_once_however_many_periods_it_missed() {
    let period = Duration::from_secs(10);
    let ran_at = Instant::now();
    let late = ran_at
        .checked_add(Duration::from_secs(35))
        .expect("an instant 35s from now");
    assert_eq!(
        due_in(period, ran_at, late),
        Duration::ZERO,
        "a pass that overran by three periods asked for more than one catch-up"
    );
}

#[test]
fn a_cadence_that_is_early_waits_out_the_remainder() {
    let period = Duration::from_secs(10);
    let ran_at = Instant::now();
    let soon = ran_at
        .checked_add(Duration::from_secs(3))
        .expect("an instant 3s from now");
    assert_eq!(due_in(period, ran_at, soon), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn a_cadence_runs_no_pass_once_the_node_is_asked_to_stop() {
    let stop = CancellationToken::new();
    stop.cancel();
    let passes = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&passes);
    every(Duration::ZERO, &stop, move |_| {
        counting.fetch_add(1, Ordering::Relaxed);
    })
    .await;
    assert_eq!(
        passes.load(Ordering::Relaxed),
        0,
        "a node already stopping still ran a cadence pass"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cadence_keeps_its_state_between_passes_and_a_stop_ends_its_wait() {
    let stop = CancellationToken::new();
    let passes = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&passes);
    let stopping = stop.clone();
    // State the closure owns, carried from one pass to the next across the
    // hop to the blocking pool and back.
    let mut rounds = 0_usize;
    let cadence = tokio::spawn(async move {
        every(Duration::from_secs(3600), &stopping, move |_| {
            rounds = rounds.saturating_add(1);
            counting.store(rounds, Ordering::Relaxed);
        })
        .await;
    });
    // The clock is paused and moves only when every task is waiting, so
    // each of these sleeps lets exactly the cadence's own wait run out.
    while passes.load(Ordering::Relaxed) < 3 {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    // Half a period on, so the stop lands in the middle of a wait rather than
    // at its end: the stop has to end that wait itself.
    tokio::time::sleep(Duration::from_secs(1800)).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(1), cadence)
        .await
        .expect("a stop did not end the cadence's wait")
        .expect("the cadence task");
    assert!(passes.load(Ordering::Relaxed) >= 3);
}

#[tokio::test]
async fn a_pass_that_panics_is_raised_on_the_cadence_task() {
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    let cadence = tokio::spawn(async move {
        every(Duration::ZERO, &stopping, |_| {
            std::panic::resume_unwind(Box::new("a defect in a pass"));
        })
        .await;
    });
    let ended = cadence.await.expect_err("the panic did not reach the task");
    assert!(ended.is_panic());
}

#[test]
fn the_published_directory_answers_the_routing_question_a_read_asks() {
    // The join this whole module was built for. Until this wave the rounds
    // were written and read by nobody, so the test asserts the *reading*:
    // what a session gets back when it asks the published answer, not what
    // the greeting side put there.
    use tessari_session::Elsewhere as _;

    let mut directory = Directory::new();
    directory.heard("two.example:9080", said(), Instant::now());
    let published = Published::holding(directory);

    let found = published
        .within(Duration::from_secs(30))
        .expect("a peer one second behind is within thirty");
    assert_eq!(found.endpoint, "two.example:9080");
    assert_eq!(
        found.node, NODE,
        "the redirect must carry who is there, or it cannot be checked on arrival"
    );
}

#[test]
fn a_peer_beyond_the_bound_is_not_a_peer_the_routing_question_offers() {
    // §C-05's *exclude, never mark*, at the surface a read actually asks.
    // An implementation that answered with its freshest peer regardless
    // would turn the bound from a promise back into a hope, and the caller
    // has no way to tell the two apart.
    use tessari_session::Elsewhere as _;

    let mut directory = Directory::new();
    directory.heard("two.example:9080", said(), Instant::now());
    let published = Published::holding(directory);

    assert!(
        published.within(Duration::from_millis(500)).is_none(),
        "a copy a second behind was offered to a read that would take half of one"
    );
}

#[test]
fn a_node_that_has_greeted_nobody_offers_nowhere() {
    // The single-node case, which is every deployment that was never told
    // about peers. It must answer *not that I know of* rather than
    // inventing a candidate, because the caller turns that answer straight
    // into a refusal.
    use tessari_session::Elsewhere as _;

    let published = Published::holding(Directory::new());
    assert!(published.within(Duration::from_secs(86_400)).is_none());
}

#[test]
fn a_greeting_round_carries_previous_readings_forward() {
    let published = Published::holding(Directory::new());
    let first = Instant::now();
    published.round(|directory| directory.heard("one:9080", said(), first));

    published.round(|directory| directory.heard("two:9080", said(), first));

    let current = published.current();
    assert!(
        current.age_of("one:9080", first).is_some(),
        "the peer heard in the first round vanished in the second"
    );
    assert!(current.age_of("two:9080", first).is_some());
}

#[test]
fn a_greeting_round_publishes_nothing_until_it_is_done() {
    let published = Published::holding(Directory::new());
    let at = Instant::now();
    let during = RefCell::new(None);
    published.round(|directory| {
        directory.heard("one:9080", said(), at);
        *during.borrow_mut() = Some(Arc::clone(&published.current()));
    });
    let seen = during.borrow().clone().expect("the round ran");
    assert!(
        seen.age_of("one:9080", at).is_none(),
        "a reader saw a half-finished round"
    );
    assert!(published.current().age_of("one:9080", at).is_some());
}

#[test]
fn an_empty_catalog_names_no_peer() {
    assert!(!names_a_peer(&[], &NODE));
}

#[test]
fn a_catalog_naming_another_node_names_a_peer() {
    let declared = [named("leader", "10.0.0.2:9000", ANOTHER)];
    assert!(names_a_peer(&declared, &NODE));
}

/// The shape W260 found, and the reason this predicate is not `is_empty`.
///
/// A cluster admits a newcomer by writing a row that describes the
/// NEWCOMER, so the first row the newcomer ever collects is its own. It
/// names a member — itself — and answers nothing about who to follow, since
/// `upstream` and `greet_round` both skip it.
#[test]
fn a_catalog_holding_only_this_nodes_own_row_names_no_peer() {
    let declared = [named("joiner", "10.0.0.9:9000", NODE)];
    assert!(!names_a_peer(&declared, &NODE));
}

#[test]
fn a_row_naming_no_node_names_no_peer() {
    let declared = [peer(Roles::WRITABLE, None)];
    assert!(!names_a_peer(&declared, &NODE));
}

#[test]
fn one_row_for_somebody_else_is_enough_beside_this_nodes_own() {
    let declared = [
        named("joiner", "10.0.0.9:9000", NODE),
        named("leader", "10.0.0.2:9000", ANOTHER),
    ];
    assert!(names_a_peer(&declared, &NODE));
}

#[test]
fn a_routing_answer_carries_the_epoch_the_named_node_claimed() {
    // The value a redirect is DATED by, and the one thing this node could
    // not have produced from its own state: `said()` claims epoch 7 and this
    // node holds no leadership at all. An implementation that reached for
    // `Db::leading()` here would answer `None` and have to invent a
    // placeholder, which is how an undateable redirect gets shipped looking
    // exactly like a dated one.
    use tessari_session::Elsewhere as _;

    let mut directory = Directory::new();
    directory.heard("10.0.0.2:9000", said(), Instant::now());
    let published = Published::holding(directory);

    let peer = published
        .within(Duration::from_secs(30))
        .expect("a serving peer one second behind is inside a thirty-second bound");

    assert_eq!(peer.endpoint, "10.0.0.2:9000");
    assert_eq!(peer.node, NODE);
    assert_eq!(
        peer.epoch,
        Epoch::new(7),
        "the routing answer lost the leadership the named node published"
    );
}

// ---- G032 S4: a placed range's leader, heard and followed ---------------

fn shard(n: u32) -> Reach {
    Reach::Shard(
        NamespaceId::new(1),
        tessari_types::DatabaseId::new(1),
        tessari_types::TableId::new(1),
        tessari_types::ShardId::new(n),
    )
}

/// A greeting from `node` standing for `range`, leading it at `leading`.
fn on_a_line(node: [u8; NODE_ID_LEN], range: Reach, leading: u64) -> Hello {
    Hello {
        node,
        line: Some(crate::peer::Line {
            range,
            leading: Epoch::new(leading),
            tail: Sequence::new(3),
            tail_leadership: Epoch::new(1),
        }),
        ..following()
    }
}

#[test]
fn a_placed_ranges_leader_is_the_peer_whose_greeting_holds_its_line_live() {
    let declared = [
        named("a", "10.0.0.1:9000", NODE),
        named("b", "10.0.0.2:9000", ANOTHER),
    ];
    let heard = greeted(&[
        ("10.0.0.1:9000", on_a_line(NODE, shard(2), 3)),
        ("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0)),
    ]);
    assert_eq!(
        leader_of_range(shard(2), &declared, &heard),
        Some((NODE, "10.0.0.1:9000".to_owned()))
    );
    assert_eq!(leader_of_range(shard(3), &declared, &heard), None);
    // A lapsed line alone leads nothing — asked without a live greeting
    // beside it, which the choice of the highest epoch would absorb.
    let lapsed = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0))]);
    assert_eq!(leader_of_range(shard(2), &declared, &lapsed), None);
    // A greeting under somebody else's row names nobody.
    let crossed = greeted(&[("10.0.0.2:9000", on_a_line(NODE, shard(2), 3))]);
    assert_eq!(leader_of_range(shard(2), &declared, &crossed), None);
}

#[test]
fn a_node_hears_a_ranges_leader_only_on_that_ranges_line() {
    let declared = [
        named("a", "10.0.0.1:9000", NODE),
        named("me", "10.0.0.2:9000", ANOTHER),
    ];
    let now = Instant::now();
    let within = tessari_storage::LEASE_TTL;
    let heard = greeted(&[("10.0.0.1:9000", on_a_line(NODE, shard(2), 3))]);
    assert!(heard_a_leader_on(
        shard(2),
        ANOTHER,
        &declared,
        &heard,
        None,
        now,
        within
    ));
    assert!(!heard_a_leader_on(
        shard(3),
        ANOTHER,
        &declared,
        &heard,
        None,
        now,
        within
    ));
    // Its own greeting is not a leader it can hear.
    let mine = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 3))]);
    assert!(!heard_a_leader_on(
        shard(2),
        ANOTHER,
        &declared,
        &mine,
        None,
        now,
        within
    ));
    // And a grant to somebody else on the line is heard without a greeting.
    assert!(heard_a_leader_on(
        shard(3),
        ANOTHER,
        &declared,
        &Directory::new(),
        Some(now),
        now,
        within
    ));
}

#[test]
fn a_node_stands_for_the_range_its_own_row_places() {
    let mut placed = named("a", "10.0.0.1:9000", NODE);
    placed.leads = Some(shard(2));
    let declared = [placed, named("b", "10.0.0.2:9000", ANOTHER)];
    assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
    assert_eq!(stands_for(&declared, &ANOTHER), None);
}

/// Q-857. A node subscribed to one shard won the store line, answered a
/// read of the split table from its one shard, and said nothing: a leader
/// never collects, so nothing on it says it holds less than everything.
#[test]
fn a_node_subscribed_to_less_than_the_store_does_not_stand_for_the_store() {
    let mut narrow = named("a", "10.0.0.1:9000", NODE);
    narrow.replicates = Some(shard(3));
    narrow.leads = Some(shard(3));
    let mut whole = named("b", "10.0.0.2:9000", ANOTHER);
    whole.replicates = Some(Reach::Store);
    let declared = [narrow, whole];
    assert!(!stands_for_the_store(&declared, &NODE));
    assert_eq!(stands_for(&declared, &NODE), Some(shard(3)));
    assert!(stands_for_the_store(&declared, &ANOTHER));
    // A namespace is narrower than the store too.
    let mut namespace = named("a", "10.0.0.1:9000", NODE);
    namespace.replicates = Some(Reach::Namespace(NamespaceId::new(1)));
    assert!(!stands_for_the_store(&[namespace], &NODE));
    // A row with no subscription stands as it always has.
    assert!(stands_for_the_store(
        &[named("a", "10.0.0.1:9000", NODE)],
        &NODE
    ));
}

/// G034 S3.2 (Q-797). Two rows bound to one node, each placing a range:
/// the node stands for the first row's range only, so it never leads two
/// placed lines. A commit spanning two placed lines therefore cannot be
/// taken by one node, and a node's own database or namespace log is written
/// only under the store line or a coarser placement it leads — both of
/// which the collectors already ask it for.
#[test]
fn a_node_bound_to_two_placing_rows_stands_for_one_range() {
    let mut first = named("a", "10.0.0.1:9000", NODE);
    first.leads = Some(shard(2));
    let mut second = named("c", "10.0.0.1:9000", NODE);
    second.leads = Some(shard(3));
    let declared = [first, named("b", "10.0.0.2:9000", ANOTHER), second];
    assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
}

/// ADR-0098 D3. A row giving its range back still holds the line and says
/// so, but no longer campaigns; the store line's leader, placed nowhere,
/// campaigns for it instead — and a store leader with a placement of its
/// own does not, so no node ever leads two placed lines.
#[test]
fn a_released_range_is_campaigned_for_by_the_store_leader_alone() {
    let mut giving = named("a", "10.0.0.1:9000", NODE);
    giving.leads = Some(shard(2));
    giving.releasing = true;
    let declared = [giving.clone(), named("b", "10.0.0.2:9000", ANOTHER)];
    assert_eq!(stands_for(&declared, &NODE), Some(shard(2)));
    assert_eq!(campaigns_for(&declared, &NODE), None);
    assert_eq!(released(&declared), vec![shard(2)]);
    assert_eq!(campaign_line(&declared, &NODE, false), None);
    assert_eq!(campaign_line(&declared, &ANOTHER, true), Some(shard(2)));
    assert_eq!(campaign_line(&declared, &ANOTHER, false), None);
    // Placed elsewhere itself, the store leader takes nothing back.
    let mut placed = named("b", "10.0.0.2:9000", ANOTHER);
    placed.leads = Some(shard(3));
    let declared = [giving.clone(), placed];
    assert_eq!(campaign_line(&declared, &ANOTHER, true), Some(shard(3)));
    // Another row still placing the range: it is not released at all.
    let mut staying = named("b", "10.0.0.2:9000", ANOTHER);
    staying.leads = Some(shard(2));
    assert!(released(&[giving, staying]).is_empty());
}

/// G053 SG5b. A non-preferred leader hands the range to a preferred
/// candidate that is audible and caught up — and to nobody else.
#[test]
fn a_leader_yields_only_to_a_preferred_candidate_that_is_caught_up() {
    let mut leading = named("a", "10.0.0.1:9000", NODE);
    leading.leads = Some(shard(2));
    let mut preferred = named("b", "10.0.0.2:9000", ANOTHER);
    preferred.leads = Some(shard(2));
    preferred.preferred = true;
    let declared = [leading.clone(), preferred.clone()];
    let now = Instant::now();
    let within = Duration::from_secs(2);
    let mine = crate::grant::Reached {
        leadership: Epoch::new(1),
        tail: Sequence::new(3),
    };
    let level = greeted(&[("10.0.0.2:9000", on_a_line(ANOTHER, shard(2), 0))]);
    assert_eq!(
        preferred_to_yield_to(shard(2), NODE, &declared, &level, mine, (now, within)),
        Some(ANOTHER)
    );
    // Behind this node's own position: not yet.
    let ahead = crate::grant::Reached {
        leadership: Epoch::new(1),
        tail: Sequence::new(4),
    };
    assert_eq!(
        preferred_to_yield_to(shard(2), NODE, &declared, &level, ahead, (now, within)),
        None
    );
    // Not heard at all, or heard too long ago.
    assert_eq!(
        preferred_to_yield_to(
            shard(2),
            NODE,
            &declared,
            &Directory::new(),
            mine,
            (now, within)
        ),
        None
    );
    let later = now
        .checked_add(Duration::from_secs(3))
        .expect("an instant three seconds on is representable");
    assert_eq!(
        preferred_to_yield_to(shard(2), NODE, &declared, &level, mine, (later, within)),
        None
    );
    // A preferred leader yields to nobody; nor does anybody when no row
    // is preferred.
    let mut both = leading.clone();
    both.preferred = true;
    assert_eq!(
        preferred_to_yield_to(
            shard(2),
            NODE,
            &[both, preferred.clone()],
            &level,
            mine,
            (now, within)
        ),
        None
    );
    let mut plain = preferred;
    plain.preferred = false;
    assert_eq!(
        preferred_to_yield_to(
            shard(2),
            NODE,
            &[leading, plain],
            &level,
            mine,
            (now, within)
        ),
        None
    );
}
