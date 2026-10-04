use super::*;

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
