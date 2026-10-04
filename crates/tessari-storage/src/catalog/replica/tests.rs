use super::{Greeter, JoinTicket, ReplicaDefinition, the_row_a_greeting_binds};
use tessari_encoding::{NODE_ID_LEN, Roles};

const GREETER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
const SOMEBODY_ELSE: [u8; NODE_ID_LEN] = [7; NODE_ID_LEN];
const PRESENTED: &str = "aa";
const TOKEN: &str = "bb";
const NOW: i64 = 1_000;

/// A row an operator declared with `NODE`.
fn bound(name: &str, node: [u8; NODE_ID_LEN]) -> ReplicaDefinition {
    ReplicaDefinition {
        node: Some(node),
        ..unbound(name)
    }
}

/// A row an operator declared without `NODE`, and without saying who may
/// bind it.
///
/// The names differ per row throughout, so that a rule taking the wrong
/// candidate is detectable rather than accidentally right.
fn unbound(name: &str) -> ReplicaDefinition {
    ReplicaDefinition {
        name: name.to_owned(),
        endpoint: "10.0.0.2:9000".to_owned(),
        roles: Roles::SERVING,
        node: None,
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

fn pinned(name: &str, fingerprint: &str) -> ReplicaDefinition {
    ReplicaDefinition {
        fingerprint: Some(fingerprint.to_owned()),
        ..unbound(name)
    }
}

fn waiting(name: &str, digest: &str, expires_ms: i64) -> ReplicaDefinition {
    ReplicaDefinition {
        join: Some(JoinTicket {
            digest: digest.to_owned(),
            expires_ms,
        }),
        ..unbound(name)
    }
}

fn greeter(token: Option<&'static str>) -> Greeter<'static> {
    Greeter {
        node: GREETER,
        fingerprint: PRESENTED,
        token,
    }
}

fn binds(declared: &[ReplicaDefinition], token: Option<&'static str>) -> Option<String> {
    the_row_a_greeting_binds(declared, &greeter(token), NOW).map(str::to_owned)
}

/// R-15: the row an operator left open is no longer handed to whichever
/// peer greets first.
/// G057 C3: a region is kept when stated, and a row that states none
/// stores no field for it — the bytes a row had before regions existed.
#[test]
fn a_region_is_stored_only_when_stated_and_read_back() {
    let plain = unbound("plain");
    assert!(!format!("{:?}", plain.to_value()).contains("region"));
    assert_eq!(
        ReplicaDefinition::from_value(&plain.to_value()).ok(),
        Some(plain)
    );
    let placed = ReplicaDefinition {
        region: Some("eu".to_owned()),
        ..unbound("placed")
    };
    assert_eq!(
        ReplicaDefinition::from_value(&placed.to_value()).ok(),
        Some(placed)
    );
}

#[test]
fn a_row_that_says_nothing_about_its_node_binds_nobody() {
    let declared = [bound("leader", SOMEBODY_ELSE), unbound("joiner")];
    assert_eq!(binds(&declared, None), None);
    assert_eq!(binds(&declared, Some(TOKEN)), None);
}

#[test]
fn a_pinned_certificate_binds_its_row_and_no_other() {
    let declared = [unbound("open"), pinned("joiner", PRESENTED)];
    assert_eq!(binds(&declared, None).as_deref(), Some("joiner"));
    let elsewhere = [pinned("joiner", "cc")];
    assert_eq!(binds(&elsewhere, None), None);
}

#[test]
fn a_join_token_binds_its_row_until_it_expires() {
    let declared = [unbound("open"), waiting("joiner", TOKEN, NOW + 1)];
    assert_eq!(binds(&declared, Some(TOKEN)).as_deref(), Some("joiner"));
    assert_eq!(binds(&declared, None), None, "no token carried");
    assert_eq!(binds(&declared, Some("cc")), None, "another token");
    let expired = [waiting("joiner", TOKEN, NOW)];
    assert_eq!(binds(&expired, Some(TOKEN)), None, "expired at now");
}

#[test]
fn an_empty_catalog_binds_nothing_because_there_is_no_row_to_bind() {
    assert_eq!(binds(&[], Some(TOKEN)), None);
}

/// The peer is already known, and knowing it twice is worse than once.
#[test]
fn a_greeting_from_a_node_a_row_already_names_binds_nothing() {
    let declared = [bound("leader", GREETER), pinned("joiner", PRESENTED)];
    assert_eq!(binds(&declared, None), None);
}

/// Evidence that approves two rows chooses neither: binding one would be
/// a guess, and a wrong guess runs a node under another row's roles.
#[test]
fn evidence_matching_two_rows_binds_neither() {
    let declared = [pinned("joiner", PRESENTED), pinned("another", PRESENTED)];
    assert_eq!(binds(&declared, None), None);
}
