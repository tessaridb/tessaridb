use std::thread::JoinHandle;

use tessari_encoding::NODE_ID_LEN;

use super::{PeerKeys, Revoked};
use crate::collection::NoLog;
use crate::error::{Error, Result};
use crate::grant::Deciding;
use crate::link::tests::{Authority, hello, keys, settled};
use crate::link::{Ask, Credential, Met, Peers, call};
use crate::peer::Purpose;

const DOOR: [u8; NODE_ID_LEN] = [7_u8; NODE_ID_LEN];
const CALLER: [u8; NODE_ID_LEN] = [8_u8; NODE_ID_LEN];

/// A door answering with `held`, greeting one caller on its own thread.
fn serving(held: &PeerKeys) -> (std::net::SocketAddr, JoinHandle<Result<Met>>) {
    let peers = Peers::bind("127.0.0.1:0", held).expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let greeting = std::thread::spawn(move || {
        peers.greet(
            || Ok(hello(DOOR)),
            &DOOR,
            &Deciding::holding(settled()),
            &NoLog,
        )
    });
    (address, greeting)
}

fn dial(address: std::net::SocketAddr, caller: &PeerKeys) -> Result<()> {
    call(address, caller, DOOR, &hello(CALLER), Ask::Nothing).map(|_| ())
}

fn only(fingerprint: String) -> Revoked {
    Revoked::from([fingerprint])
}

/// The refusal a revoked certificate gets, by name — not any failure.
///
/// By the text and not the variant: the door reports a failed handshake as
/// a transport failure and a dial as the socket's, and both carry rustls's
/// own reason.
fn names_revocation(failure: &Error) -> bool {
    failure
        .to_string()
        .contains("invalid peer certificate: Revoked")
}

#[test]
fn a_certificate_admitted_once_is_judged_again_after_a_revocation_or_a_removal() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let caller = authority.keys(CALLER, Purpose::Peer);
    let presented = caller.duplicate().chain.remove(0);
    assert!(door.still_admits(&presented));

    door.refuse(only(caller.fingerprint()));
    assert!(!door.still_admits(&presented), "a revoked certificate");

    door.refuse(Revoked::new());
    assert!(door.still_admits(&presented), "the revocation lifted");
    door.refuse_nodes(super::Removed::from([CALLER]));
    assert!(
        !door.still_admits(&presented),
        "a removed node's certificate"
    );
}

/// Q-901: a stream opened before its certificate's `notAfter` ends at it,
/// as one presenting a revoked certificate does — a handshake is the only
/// other place the date is read, and a held stream never makes another.
#[test]
fn a_certificate_admitted_once_is_judged_again_after_it_expires() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let current = authority.issue(CALLER, Purpose::Peer).chain.remove(0);
    let lapsed = authority.expired(CALLER, Purpose::Peer).chain.remove(0);
    assert!(door.still_admits(&current), "a certificate in its window");
    assert!(
        !door.still_admits(&lapsed),
        "a certificate past its notAfter"
    );
}

#[test]
fn a_door_refuses_a_caller_whose_certificate_is_revoked() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let caller = authority.keys(CALLER, Purpose::Peer);
    door.refuse(only(caller.fingerprint()));

    let (address, greeting) = serving(&door);
    drop(dial(address, &caller));
    let refused = greeting
        .join()
        .expect("the door's thread")
        .expect_err("a revoked caller is not admitted");
    assert!(names_revocation(&refused), "{refused}");
}

#[test]
fn a_caller_refuses_a_door_whose_certificate_is_revoked() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let caller = authority.keys(CALLER, Purpose::Peer);
    caller.refuse(only(door.fingerprint()));

    let (address, greeting) = serving(&door);
    let refused = dial(address, &caller).expect_err("a revoked door is not spoken to");
    assert!(names_revocation(&refused), "{refused}");
    drop(greeting.join());
}

#[test]
fn a_door_refuses_a_removed_node_whatever_certificate_it_holds() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    door.refuse_nodes(super::Removed::from([CALLER]));

    // A certificate issued after the removal is still the removed node's.
    let caller = authority.keys(CALLER, Purpose::Peer);
    let (address, greeting) = serving(&door);
    drop(dial(address, &caller));
    let refused = greeting
        .join()
        .expect("the door's thread")
        .expect_err("a removed node is not admitted");
    assert!(names_revocation(&refused), "{refused}");

    // The control: another node is admitted by the same door.
    const OTHER: [u8; NODE_ID_LEN] = [6_u8; NODE_ID_LEN];
    let other = authority.keys(OTHER, Purpose::Peer);
    let (address, greeting) = serving(&door);
    call(address, &other, DOOR, &hello(OTHER), Ask::Nothing).expect("another node");
    greeting
        .join()
        .expect("the door's thread")
        .expect("another node is admitted");
}

#[test]
fn a_replaced_credential_is_what_the_next_dial_presents() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let caller = authority.keys(CALLER, Purpose::Peer);
    // The door refuses the caller's first certificate, so only the new one
    // can be what gets the second dial in.
    door.refuse(only(caller.fingerprint()));
    let (address, greeting) = serving(&door);
    drop(dial(address, &caller));
    drop(greeting.join());

    caller
        .replace(authority.issue(CALLER, Purpose::Peer))
        .expect("a credential the authority issued");
    let (address, greeting) = serving(&door);
    dial(address, &caller).expect("the new certificate is presented and admitted");
    let met = greeting
        .join()
        .expect("the door's thread")
        .expect("the door admits the new certificate");
    assert_eq!(met.said.node, CALLER);
}

#[test]
fn a_door_presents_its_replaced_credential_from_the_next_handshake() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let caller = authority.keys(CALLER, Purpose::Peer);
    caller.refuse(only(door.fingerprint()));
    // One door, held across the replacement: the bind is not redone, so
    // what changes is what the door resolves at the handshake.
    let peers = Peers::bind("127.0.0.1:0", &door).expect("a peer door on loopback");
    let address = peers.address().expect("the door's address");
    let answering = std::thread::spawn(move || {
        let deciding = Deciding::holding(settled());
        let first = peers.greet(|| Ok(hello(DOOR)), &DOOR, &deciding, &NoLog);
        let second = peers.greet(|| Ok(hello(DOOR)), &DOOR, &deciding, &NoLog);
        (first, second)
    });

    let refused = dial(address, &caller).expect_err("the old certificate is refused");
    assert!(names_revocation(&refused), "{refused}");
    door.replace(authority.issue(DOOR, Purpose::Peer))
        .expect("a credential the authority issued");
    dial(address, &caller).expect("the door now presents its new certificate");
    let (_, second) = answering.join().expect("the door's thread");
    assert_eq!(
        second.expect("the second caller is admitted").said.node,
        CALLER
    );
}

#[test]
fn a_replacement_whose_key_is_not_the_certificates_is_refused_and_changes_nothing() {
    let authority = Authority::new();
    let held = authority.keys(CALLER, Purpose::Peer);
    let before = held.fingerprint();
    let other = authority.issue(CALLER, Purpose::Peer);
    let mismatched = Credential {
        chain: authority.issue(CALLER, Purpose::Peer).chain,
        key: other.key,
    };

    let refused = held
        .replace(mismatched)
        .expect_err("a key that is not the leaf's");
    assert!(
        matches!(&refused, Error::Transport(why) if why.contains("KeyMismatch")),
        "{refused}"
    );
    assert_eq!(held.fingerprint(), before, "the old credential is kept");

    let door = authority.keys(DOOR, Purpose::Peer);
    let (address, greeting) = serving(&door);
    dial(address, &held).expect("the kept credential still works");
    drop(greeting.join());
}

#[test]
fn an_expired_certificate_is_refused_at_the_handshake() {
    let authority = Authority::new();
    let door = authority.keys(DOOR, Purpose::Peer);
    let expired = keys(authority.expired(CALLER, Purpose::Peer), &authority.der())
        .expect("an expired credential is still a matching pair");

    let (address, greeting) = serving(&door);
    drop(dial(address, &expired));
    let refused = greeting
        .join()
        .expect("the door's thread")
        .expect_err("an expired certificate is not admitted");
    assert!(
        matches!(&refused, Error::Transport(why) if why.to_lowercase().contains("expired")),
        "{refused}"
    );
}
