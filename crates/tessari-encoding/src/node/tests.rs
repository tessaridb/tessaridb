#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn the_exact_version_begins_with_the_ordered_one_and_says_more() {
    // The relationship between the two forms, pinned. `BUILD_VERSION` may
    // carry a suffix the ordered form drops, but it may never disagree
    // about the three numbers themselves — a build whose command line and
    // whose stored identity named different versions would send somebody
    // looking for a bug in the wrong release.
    let ordered = NodeVersion::current().to_string();
    assert!(
        BUILD_VERSION.starts_with(&ordered),
        "the exact version {BUILD_VERSION} does not begin with the ordered one {ordered}"
    );
    let rest = BUILD_VERSION.strip_prefix(&ordered).unwrap_or_default();
    assert!(
        rest.is_empty() || rest.starts_with('-') || rest.starts_with('+'),
        "the exact version {BUILD_VERSION} carries {rest:?} after the numbers, \
             which is neither a pre-release nor build metadata"
    );
}

#[test]
fn a_pre_release_is_visible_where_a_person_reads_it_and_invisible_where_it_is_compared() {
    // This is the whole point of holding two forms, so it is asserted on
    // whichever kind of build is running rather than only on a
    // pre-release. Both branches are real: the assertion that matters flips
    // when the suffix goes away, and a test that only held for one of them
    // would go quiet exactly when the release it was written for shipped.
    let ordered = NodeVersion::current().to_string();
    if BUILD_VERSION == ordered {
        assert!(
            !BUILD_VERSION.contains('-'),
            "a final release carries no pre-release suffix"
        );
    } else {
        assert!(
            BUILD_VERSION.contains('-'),
            "{BUILD_VERSION} differs from the ordered form {ordered} \
                 without carrying a pre-release suffix"
        );
        assert!(
            !ordered.contains('-'),
            "the ordered form {ordered} kept a suffix it cannot compare"
        );
    }
}

#[test]
fn every_role_that_can_be_written_can_be_read_back() {
    // The property the shared table buys, asserted rather than assumed: a
    // role the answer names is a role a statement can set. Two hand-kept
    // lists would drift here silently, and the symptom would be an operator
    // told a role exists and refused when they write it.
    for name in Roles::SERVING
        .and(Roles::WRITABLE)
        .and(Roles::COORDINATING)
        .names()
    {
        let parsed = Roles::parse(name).expect("a reported role must parse");
        assert_eq!(parsed.names(), vec![name]);
    }
    assert!(Roles::parse("leader").is_none());
    assert!(Roles::parse("Serving").is_none(), "matching is exact");
}

#[test]
fn no_roles_is_a_state_and_not_a_missing_answer() {
    assert!(Roles::NONE.names().is_empty());
    assert!(!Roles::NONE.has(Roles::SERVING));
    assert_eq!(Roles::NONE.and(Roles::SERVING), Roles::SERVING);
}

#[test]
fn an_identity_round_trips_through_its_bytes() {
    let original = NodeIdentity {
        id: [0x5a; NODE_ID_LEN],
        roles: Roles::ALONE.and(Roles::COORDINATING),
        membership: Membership::Alone,
        version: NodeVersion {
            major: 3,
            minor: 14,
            patch: 159,
        },
        endpoints: vec!["127.0.0.1:8000".to_owned(), "[::1]:8001".to_owned()],
    };
    let encoded = original.encode();
    assert_eq!(NodeIdentity::decode(encoded.as_slice()).unwrap(), original);
}

#[test]
fn a_node_standing_alone_serves_and_writes_and_does_not_coordinate() {
    let identity = NodeIdentity::alone([1; NODE_ID_LEN]);
    assert!(identity.roles.has(Roles::SERVING));
    assert!(identity.roles.has(Roles::WRITABLE));
    assert!(!identity.roles.has(Roles::COORDINATING));
    assert_eq!(identity.roles.names(), vec!["serving", "writable"]);
}

#[test]
fn an_empty_endpoint_list_round_trips_as_an_empty_list() {
    // The arm that would otherwise be tested only by the arm that has data:
    // a terminated encoding with nothing in it must decode to nothing, not
    // to one empty string.
    let original = NodeIdentity::alone([7; NODE_ID_LEN]);
    let encoded = original.encode();
    let found = NodeIdentity::decode(encoded.as_slice()).unwrap();
    assert!(found.endpoints.is_empty());
    assert_eq!(found, original);
}

#[test]
fn a_role_bit_this_build_does_not_know_is_refused_rather_than_ignored() {
    let mut bytes = NodeIdentity::alone([2; NODE_ID_LEN]).encode().into_bytes();
    // Header (2) + revision (1) + id (16) lands on the roles byte.
    bytes[19] |= 0b1000_0000;
    assert!(NodeIdentity::decode(&bytes).is_err());
}

#[test]
fn the_id_renders_as_the_hex_an_operator_reads() {
    let identity = NodeIdentity::alone([0xab; NODE_ID_LEN]);
    assert_eq!(identity.record_id().to_string(), "ab".repeat(NODE_ID_LEN));
}
