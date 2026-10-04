use super::{Destination, Directory};
use core::cell::RefCell;
use core::time::Duration;
use std::time::Instant;
use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::ReplicaDefinition;
use tessari_types::{Epoch, Sequence};

use crate::peer::Hello;

const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const ANOTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
const THIRD: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

/// A declared peer row: a name, where it answers, and who is there.
fn declared(id: u32, endpoint: &str, node: Option<[u8; NODE_ID_LEN]>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: format!("peer{id}"),
        endpoint: endpoint.to_owned(),
        roles: Roles::SERVING,
        node,
        // Routing is not subscription: which peers this node greets is a
        // different question from what those peers may collect, so these
        // rows deliberately grant nothing.
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

/// A greeting function that records every endpoint it was asked to dial,
/// and refuses the ones named in `silent`.
fn greeter<'a>(
    dialled: &'a RefCell<Vec<String>>,
    silent: &'a [&'a str],
) -> impl Fn(&str, [u8; NODE_ID_LEN]) -> Result<Hello, ()> + 'a {
    move |endpoint, node| {
        dialled.borrow_mut().push(endpoint.to_owned());
        if silent.contains(&endpoint) {
            return Err(());
        }
        Ok(said(node, Some(Duration::from_secs(1)), true))
    }
}

/// A greeting from `node`, saying its copy is `age` old and that it `serves`.
fn said(node: [u8; NODE_ID_LEN], age: Option<Duration>, serves: bool) -> Hello {
    Hello {
        node,
        build: NodeVersion {
            major: 0,
            minor: 1,
            patch: 1,
        },
        epoch: Epoch::new(7),
        roles: if serves { Roles::SERVING } else { Roles::NONE },
        tail: Sequence::new(4096),
        tail_leadership: Epoch::new(1),
        current_as_of: age,
        policy: None,
        line: None,
    }
}

#[test]
fn the_leader_of_a_line_is_the_peer_heard_leading_it_at_the_newest_epoch() {
    let shard = tessari_types::Reach::Shard(
        tessari_types::NamespaceId::new(1),
        tessari_types::DatabaseId::new(1),
        tessari_types::TableId::new(1),
        tessari_types::ShardId::new(1),
    );
    let line = |leading: u64| crate::peer::Line {
        range: shard,
        leading: Epoch::new(leading),
        tail: Sequence::new(1),
        tail_leadership: Epoch::new(1),
    };
    let mut directory = Directory::new();
    let now = Instant::now();
    // An old leader still greeting at the epoch it lost, the new one, and
    // a candidate whose lease lapsed: only the newest live lease leads.
    directory.heard(
        "one:9000",
        Hello {
            line: Some(line(3)),
            ..said(ONE, None, true)
        },
        now,
    );
    directory.heard(
        "another:9000",
        Hello {
            line: Some(line(4)),
            ..said(ANOTHER, None, true)
        },
        now,
    );
    directory.heard(
        "third:9000",
        Hello {
            line: Some(line(0)),
            ..said(THIRD, None, true)
        },
        now,
    );
    assert_eq!(
        directory.leading(shard),
        Some(("another:9000".to_owned(), ANOTHER, Epoch::new(4)))
    );
    assert_eq!(directory.leading(tessari_types::Reach::Store), None);
}

/// A directory holding one serving peer at `two.example:9080`, five seconds
/// old when it was heard.
fn one_peer(heard_at: Instant) -> Directory {
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        said(ANOTHER, Some(Duration::from_secs(5)), true),
        heard_at,
    );
    directory
}

#[test]
fn a_remembered_reading_ages_with_the_clock() {
    // The rule this module exists for. The peer said five seconds; thirty
    // seconds later its copy is thirty-five seconds old, not five. A router
    // that answered five would be measuring staleness with a stale
    // measurement, which is the failure the bound exists to prevent.
    let heard_at = Instant::now();
    let directory = one_peer(heard_at);

    assert_eq!(
        directory.age_of("two.example:9080", heard_at),
        Some(Duration::from_secs(5)),
        "at the instant it was heard, the age is what the peer said"
    );
    assert_eq!(
        directory.age_of(
            "two.example:9080",
            heard_at
                .checked_add(Duration::from_secs(30))
                .expect("thirty seconds after an instant this process made")
        ),
        Some(Duration::from_secs(35)),
        "the remembered reading did not age"
    );
    assert_eq!(
        directory.age_of("nobody.example:9080", heard_at),
        None,
        "an address nobody has greeted from reported an age"
    );
}

/// A greeting from a node that claims the writable role at `epoch`.
fn leads(node: [u8; NODE_ID_LEN], epoch: u64) -> Hello {
    let mut hello = said(node, Some(Duration::from_secs(1)), true);
    hello.roles = Roles::SERVING.and(Roles::WRITABLE);
    hello.epoch = Epoch::new(epoch);
    hello
}

#[test]
fn a_directory_of_followers_knows_of_no_leader() {
    // The answer that matters most, because the alternative is a read that
    // asked for the leader being sent to a node that never claimed to be
    // one. `None` here becomes a refusal upstream, which is the honest end.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        said(ANOTHER, Some(Duration::ZERO), true),
        heard_at,
    );

    assert_eq!(
        directory.writable(),
        None,
        "a peer at zero lag was read as a leader; level is not authoritative"
    );
}

#[test]
fn the_peer_that_claims_the_writable_role_is_the_one_named() {
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        said(ANOTHER, Some(Duration::ZERO), true),
        heard_at,
    );
    directory.heard("three.example:9080", leads(THIRD, 9), heard_at);

    assert_eq!(
        directory.writable(),
        Some(("three.example:9080".to_owned(), THIRD)),
        "both halves travel, or the redirect cannot be checked on arrival"
    );
}

#[test]
fn two_peers_claiming_the_leadership_resolve_to_the_later_epoch() {
    // Not a malformed directory: this is what a handover looks like from
    // outside while one greeting is newer than the other. The higher epoch
    // is the later claim.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard("two.example:9080", leads(ANOTHER, 7), heard_at);
    directory.heard("three.example:9080", leads(THIRD, 9), heard_at);

    assert_eq!(
        directory.writable(),
        Some(("three.example:9080".to_owned(), THIRD)),
    );
}

#[test]
fn an_equal_epoch_resolves_the_same_way_twice() {
    // The map is ordered, so a tie has to break deterministically or the
    // same question answers differently on two runs.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard("three.example:9080", leads(THIRD, 9), heard_at);
    directory.heard("two.example:9080", leads(ANOTHER, 9), heard_at);

    assert_eq!(
        directory.writable(),
        Some(("three.example:9080".to_owned(), THIRD)),
        "a tie went to the lexicographically later endpoint"
    );
}

#[test]
fn a_peer_that_cannot_say_how_old_its_copy_is_is_outside_every_bound() {
    // `None` is a real answer, not an omission, and it gets the same
    // treatment the local `None` already gets: excluded, never marked.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard("two.example:9080", said(ANOTHER, None, true), heard_at);

    assert_eq!(directory.age_of("two.example:9080", heard_at), None);
    assert_eq!(
        directory.read_within(None, Duration::from_secs(86_400), heard_at),
        Destination::Nowhere,
        "a copy of unknown age was admitted by a bound of a whole day, which \
             would make the bound a formality rather than a guarantee"
    );
}

#[test]
fn a_bound_this_node_meets_is_answered_here_and_never_redirected() {
    // Here first: a redirect this node did not need costs the client a round
    // trip and hands it a node it had no reason to learn about. The peer in
    // this directory is FRESHER than we are, and is still not named.
    let heard_at = Instant::now();
    let directory = one_peer(heard_at);

    assert_eq!(
        directory.read_within(
            Some(Duration::from_secs(20)),
            Duration::from_secs(30),
            heard_at
        ),
        Destination::Here,
    );
}

#[test]
fn a_bound_this_node_misses_names_a_peer_that_meets_it() {
    // §C-07 settled that no node proxies, so a redirect must NAME a peer --
    // and it names both halves, because an address alone could not be
    // checked on arrival.
    let heard_at = Instant::now();
    let directory = one_peer(heard_at);

    assert_eq!(
        directory.read_within(
            Some(Duration::from_secs(90)),
            Duration::from_secs(30),
            heard_at
        ),
        Destination::There {
            endpoint: "two.example:9080".to_owned(),
            node: ANOTHER,
        },
    );
}

#[test]
fn a_peer_that_does_not_serve_is_never_named() {
    // A node drained for maintenance still holds data and still greets.
    // Sending a client there is precisely what draining exists to prevent,
    // so its currency is irrelevant -- and here it is the best in the room.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard(
        "drained.example:9080",
        said(ANOTHER, Some(Duration::ZERO), false),
        heard_at,
    );

    assert_eq!(
        directory.read_within(None, Duration::from_secs(30), heard_at),
        Destination::Nowhere,
        "a drained node was offered to a client",
    );
}

#[test]
fn a_bound_no_copy_meets_is_answered_nowhere() {
    // Refused rather than promoted to whoever happens to be freshest. The
    // peer here is only a little outside the bound, which is the case a
    // router would be most tempted to round in its own favour.
    let heard_at = Instant::now();
    let directory = one_peer(heard_at);

    assert_eq!(
        directory.read_within(
            Some(Duration::from_secs(600)),
            Duration::from_secs(4),
            heard_at
        ),
        Destination::Nowhere,
    );
}

#[test]
fn the_freshest_peer_within_the_bound_is_the_one_named() {
    // Deterministic, so the choice is assertable, and the one most likely
    // still inside the bound when the client arrives -- the reading goes on
    // ageing while the client travels. The older peer is named FIRST in the
    // ordered map, so a selection that simply took the first match would
    // pass every other test in this module and fail this one.
    let heard_at = Instant::now();
    let mut directory = Directory::new();
    directory.heard(
        "a-older.example:9080",
        said(ONE, Some(Duration::from_secs(25)), true),
        heard_at,
    );
    directory.heard(
        "b-fresher.example:9080",
        said(ANOTHER, Some(Duration::from_secs(2)), true),
        heard_at,
    );

    assert_eq!(
        directory.read_within(None, Duration::from_secs(30), heard_at),
        Destination::There {
            endpoint: "b-fresher.example:9080".to_owned(),
            node: ANOTHER,
        },
    );
}

#[test]
fn a_peer_that_did_not_answer_keeps_ageing_rather_than_vanishing() {
    // The rule the wave exists for. A peer greeted once and then silent must
    // keep the reading it already gave, so that it drifts out of tighter
    // bounds first and looser ones later. Erasing it would make its age
    // unknown, and an unknown age is outside EVERY bound — so one dropped
    // greeting would take a healthy node out of all routing at once.
    let heard_at = Instant::now();
    let mut directory = one_peer(heard_at);
    let later = heard_at
        .checked_add(Duration::from_secs(30))
        .expect("the clock moves forward");

    let dialled = RefCell::new(Vec::new());
    let reached = directory.greet_round(
        &[declared(1, "two.example:9080", Some(ANOTHER))],
        &ONE,
        later,
        greeter(&dialled, &["two.example:9080"]),
    );

    assert_eq!(reached, 0, "the peer refused, so nothing was reached");
    assert_eq!(
        directory.age_of("two.example:9080", later),
        Some(Duration::from_secs(35)),
        "the silent peer's reading should have aged, not vanished"
    );
}

#[test]
fn one_silent_peer_does_not_end_the_round() {
    // The peer after the failing one is the one most likely to still be
    // serving, so a round that returned at the first refusal would punish
    // every peer for the misfortune of being declared later.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[
            declared(1, "silent.example:9080", Some(ANOTHER)),
            declared(2, "awake.example:9080", Some(THIRD)),
        ],
        &ONE,
        now,
        greeter(&dialled, &["silent.example:9080"]),
    );

    assert_eq!(reached, 1, "one of the two answered");
    assert_eq!(
        dialled.borrow().as_slice(),
        ["silent.example:9080", "awake.example:9080"],
        "both peers should have been attempted"
    );
    assert!(
        directory.at("awake.example:9080").is_some(),
        "the peer after the failing one should have been recorded"
    );
}

#[test]
fn a_peer_whose_row_names_no_node_is_not_dialled() {
    // Opening a session derives the peer's TLS name from its generated
    // identifier, so a row that names no node cannot be reached at all. The
    // round skips it rather than refusing the pass: it cannot invent an id,
    // and one incomplete declaration must not disable routing for everyone.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[
            declared(1, "unbound.example:9080", None),
            declared(2, "bound.example:9080", Some(ANOTHER)),
        ],
        &ONE,
        now,
        greeter(&dialled, &[]),
    );

    assert_eq!(reached, 1, "only the bound row was diallable");
    assert_eq!(
        dialled.borrow().as_slice(),
        ["bound.example:9080"],
        "the unbound row should never have been dialled"
    );
    assert!(
        directory.at("unbound.example:9080").is_none(),
        "a row that was never dialled has nothing to record"
    );
}

#[test]
fn a_node_does_not_greet_itself() {
    // This node's own row sits in the same catalog as everybody else's. Its
    // currency is already known directly, so dialling itself is a round trip
    // to learn what the store answers for free — and it would put this node
    // in its own directory, where `read_within`'s *here first* rule has
    // already decided it does not belong.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[
            declared(1, "me.example:9080", Some(ONE)),
            declared(2, "other.example:9080", Some(ANOTHER)),
        ],
        &ONE,
        now,
        greeter(&dialled, &[]),
    );

    assert_eq!(reached, 1, "only the other node was greeted");
    assert_eq!(
        dialled.borrow().as_slice(),
        ["other.example:9080"],
        "this node should not have dialled itself"
    );
    assert!(
        directory.at("me.example:9080").is_none(),
        "this node must stay out of its own directory"
    );
}

#[test]
fn a_round_records_what_each_peer_said_against_the_instant_it_was_heard() {
    // The round's whole product. A greeting recorded without its instant is
    // a claim with no date, and the ageing rule has nothing to work from.
    let now = Instant::now();
    let mut directory = Directory::new();
    let dialled = RefCell::new(Vec::new());

    let reached = directory.greet_round(
        &[declared(1, "two.example:9080", Some(ANOTHER))],
        &ONE,
        now,
        greeter(&dialled, &[]),
    );

    assert_eq!(reached, 1);
    let heard = directory
        .at("two.example:9080")
        .expect("the peer answered, so it was recorded");
    assert_eq!(heard.said.node, ANOTHER, "the greeting names who is there");
    assert_eq!(heard.at, now, "recorded against the instant it was heard");
    assert_eq!(
        directory.age_of("two.example:9080", now),
        Some(Duration::from_secs(1)),
        "and the reading is usable the moment it lands"
    );
}

#[test]
fn a_joining_node_greets_its_seeds_because_its_catalog_names_nobody() {
    // The bootstrap round. A node just told to join holds no replica rows,
    // so `greet_round` would dial nobody at all and this node would never
    // learn anything about anything.
    let seeds = [
        crate::joining::Seed {
            node: ANOTHER,
            endpoint: "10.0.0.2:9000".to_owned(),
        },
        crate::joining::Seed {
            node: THIRD,
            endpoint: "10.0.0.3:9000".to_owned(),
        },
    ];
    let mut directory = Directory::new();
    let now = Instant::now();
    let reached = directory.greet_seeds(&seeds, &ONE, now, |_endpoint, node| {
        Ok::<_, ()>(said(node, Some(Duration::ZERO), true))
    });
    assert_eq!(reached, 2, "both seeds answered");
    assert!(directory.at("10.0.0.2:9000").is_some());
    assert!(directory.at("10.0.0.3:9000").is_some());
}

#[test]
fn a_node_seeded_with_itself_does_not_dial_itself() {
    // An operator who seeds a node with its own address has written a loop.
    // Dialling it would put this node in its own directory, where the
    // *here first* rule has already decided it does not belong.
    let seeds = [crate::joining::Seed {
        node: ONE,
        endpoint: "10.0.0.1:9000".to_owned(),
    }];
    let mut directory = Directory::new();
    let reached = directory.greet_seeds(&seeds, &ONE, Instant::now(), |_endpoint, node| {
        Ok::<_, ()>(said(node, Some(Duration::ZERO), true))
    });
    assert_eq!(reached, 0, "the one seed was this node");
    assert!(directory.at("10.0.0.1:9000").is_none());
}

#[test]
fn a_seed_that_does_not_answer_leaves_the_directory_as_it_was() {
    // One failure does not end the round and nothing is erased — the same
    // rule the declared round follows, stated here because a joining node
    // has no earlier reading to keep and the count is all it has.
    let seeds = [crate::joining::Seed {
        node: ANOTHER,
        endpoint: "10.0.0.2:9000".to_owned(),
    }];
    let mut directory = Directory::new();
    let reached = directory.greet_seeds(&seeds, &ONE, Instant::now(), |_, _| Err::<Hello, ()>(()));
    assert_eq!(reached, 0);
    assert!(directory.at("10.0.0.2:9000").is_none());
}
