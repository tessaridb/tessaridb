//! A follower below its leader's log start copies the leader's state
//! (ADR-0094 D3).
//!
//! A log bounded by default means a follower stopped for long enough comes back
//! asking for a position its leader no longer holds, and the leader answers
//! `Uncollectable`. Retrying that answer never succeeds; the repair is a copy of
//! the leader's state for this node's subscription, after which the follower
//! collects again from where the copy stood it.

use tessari_storage::Upstream;
use tessaridb::Db;

/// Whether this node leads anything: a placed range whose leader it is.
///
/// A node on the store line that collects at all is not writable, so a placed
/// range is the one thing it can still be the origin of. A copy would
/// overwrite that, so such a node is stranded rather than re-seeded.
pub(crate) fn leads_a_range(
    declared: &[tessari_storage::ReplicaDefinition],
    me: [u8; tessari_storage::NODE_ID_LEN],
    heard: &tessari_wire::Directory,
) -> bool {
    declared.iter().filter_map(|peer| peer.leads).any(|range| {
        tessari_wire::leader_of_range(range, declared, heard).is_some_and(|(node, _)| node == me)
    })
}

/// Copy the state `node` at `endpoint` holds for this node, or say why not.
///
/// Answers whether a copy landed, so the caller knows to drop its cursors: the
/// copy stood each log where the leader's stood, and a cursor still pointing
/// below that would ask for the pruned position again.
pub(crate) fn reseed(
    db: &Db,
    (mine, authority): (
        &tessari_wire::Credential,
        &tessari_wire::CertificateDer<'static>,
    ),
    (node, endpoint): ([u8; tessari_storage::NODE_ID_LEN], &str),
    said: &tessari_wire::Hello,
    leads: bool,
) -> bool {
    let store = db.store();
    if leads {
        store.upstream_is(Upstream::Stranded);
        log::warn!(
            "this node is below the log start of {endpoint} and leads a range of its own, so it \
             is not copied over: restore it from a snapshot (`tessaridb --restore`) and start it \
             again"
        );
        return false;
    }
    store.upstream_is(Upstream::Copying);
    log::info!("this node is below the log start of {endpoint}; copying its state");
    match tessari_wire::copy(endpoint, mine.duplicate(), authority, node, said, store) {
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
