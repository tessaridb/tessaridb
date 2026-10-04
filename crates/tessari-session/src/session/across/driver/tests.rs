//! When the coordinator writes two records in its range rather than four.

use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_storage::ReplicaDefinition;

use super::{MERGED_FROM, every_peer_reads_merged};

const ME: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const OTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];

fn row(name: &str, node: Option<[u8; NODE_ID_LEN]>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: name.to_owned(),
        endpoint: format!("{name}:9000"),
        roles: Roles::NONE,
        node,
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

const OLDER: NodeVersion = NodeVersion {
    major: 0,
    minor: 24,
    patch: 0,
};

#[test]
fn every_peer_on_the_new_build_merges_and_one_older_or_unheard_does_not() {
    let rows = [row("me", Some(ME)), row("other", Some(OTHER))];
    // This node's own row is never asked about: nobody greets itself.
    assert!(every_peer_reads_merged(&rows, ME, |endpoint| {
        (endpoint == "other:9000").then_some(MERGED_FROM)
    }));
    assert!(!every_peer_reads_merged(&rows, ME, |_| Some(OLDER)));
    assert!(!every_peer_reads_merged(&rows, ME, |_| None));
}

#[test]
fn a_row_not_bound_to_a_node_must_be_heard_too() {
    // A follower an operator declared by name alone still collects the log.
    let rows = [row("me", Some(ME)), row("follower", None)];
    assert!(!every_peer_reads_merged(&rows, ME, |endpoint| {
        (endpoint == "me:9000").then_some(MERGED_FROM)
    }));
    assert!(every_peer_reads_merged(&rows, ME, |_| Some(MERGED_FROM)));
}
