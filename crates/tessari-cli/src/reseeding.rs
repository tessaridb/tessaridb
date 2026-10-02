//! A follower below its leader's log start copies the leader's state
//! (ADR-0094 D3).
//!
//! A log bounded by default means a follower stopped for long enough comes back
//! asking for a position its leader no longer holds, and the leader answers
//! `Uncollectable`. Retrying that answer never succeeds; the repair is a copy of
//! the leader's state for this node's subscription, after which the follower
//! collects again from where the copy stood it.
//!
//! A follower whose copy FORKED from the line takes the same repair (ADR-0107,
//! Q-879 H2): a record it holds at a position where the leader's answer holds
//! another is met again on every pass, and the copy replaces what the line does
//! not hold.

use tessari_storage::Upstream;
use tessaridb::Db;

/// Whether this node leads anything: a placed range its own row places it to
/// lead.
///
/// A node on the store line that collects at all is not writable, so a placed
/// range is the one thing it can still be the origin of. A copy would
/// overwrite that, so such a node is stranded rather than re-seeded.
///
/// Read from the catalog's row and not from who has been heard leading: the
/// directory holds what PEERS said, never this node's own greeting, so asked
/// there the answer for this node was always no — and a copy removed the
/// records of the range it led (Q-884).
pub(crate) fn leads_a_range(
    declared: &[tessari_storage::ReplicaDefinition],
    me: [u8; tessari_storage::NODE_ID_LEN],
) -> bool {
    tessari_wire::stands_for(declared, &me).is_some()
}

/// Copy the state `node` at `endpoint` holds for this node, or say why not.
///
/// Answers whether a copy landed, so the caller knows to drop its cursors: the
/// copy stood each log where the leader's stood, and a cursor still pointing
/// below that would ask for the pruned position again.
pub(crate) fn reseed(
    db: &Db,
    keys: &tessari_wire::PeerKeys,
    (node, endpoint): ([u8; tessari_storage::NODE_ID_LEN], &str),
    said: &tessari_wire::Hello,
    leads: bool,
) -> bool {
    let store = db.store();
    if leads {
        store.upstream_is(Upstream::Stranded);
        log::warn!(
            "this node cannot continue {endpoint}'s log — it is below its start or forked from \
             it — and leads a range of its own, so it is not copied over: restore it from a \
             snapshot (`tessaridb --restore`) and start it again"
        );
        return false;
    }
    store.upstream_is(Upstream::Copying);
    log::info!(
        "this node cannot continue {endpoint}'s log — below its start or forked from it; copying \
         its state"
    );
    match tessari_wire::copy(endpoint, keys, node, said, store) {
        Ok(copied) => {
            store.replica_copied(copied.records);
            log::info!(
                "copied {} record(s) from {endpoint} and removed {}; collecting from there",
                copied.records,
                copied.removed
            );
            true
        }
        Err(why) => {
            store.upstream_is(Upstream::CopyFailed);
            log::warn!(
                "copying the state of {endpoint} failed: {why}. This node is behind, not \
                 damaged, and tries again next round."
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use tessari_storage::ReplicaDefinition;
    use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};

    use super::leads_a_range;

    /// A row naming `node`, placed to lead `leads`.
    fn row(node: Option<[u8; 16]>, leads: Option<Reach>) -> ReplicaDefinition {
        ReplicaDefinition {
            name: "peer".to_owned(),
            endpoint: "10.0.0.2:9000".to_owned(),
            roles: tessari_storage::Roles::WRITABLE,
            node,
            replicates: None,
            leads,
            clients: None,
            http: None,
            fingerprint: None,
            join: None,
        }
    }

    #[test]
    fn a_node_placed_to_lead_a_range_is_never_copied_over() {
        let me = [7; 16];
        let shard = Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            ShardId::new(1),
        );
        let leading = row(Some(me), Some(shard));
        let another = row(Some([8; 16]), Some(shard));
        assert!(leads_a_range(&[leading], me));
        assert!(!leads_a_range(&[another], me));
        assert!(!leads_a_range(&[], me));
    }
}
