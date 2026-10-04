use super::*;

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
