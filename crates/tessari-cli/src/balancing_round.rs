//! The balancing round: splitting and merging the shards of tables that asked
//! for it (ADR-0113 D2).
//!
//! A cadence of its own rather than a step of housekeeping, because a pass
//! walks shards: on a big table that is the longest thing a node does in the
//! background, and inside housekeeping it would hold up trimming the log and
//! expiring records. Only the node that may commit to the store line acts;
//! every other pass returns at once.

use tessaridb::{Db, ShardSamples};

/// Split and merge the shards of balanced tables on its cadence, until `stop`.
///
/// Says at `info` every act — each is a change to a table's shard map, which
/// an operator wants to find in the node's log beside the statement in the
/// store's — and at `warn` a refused one, which leaves a table out of bounds.
pub(crate) async fn balance_shards(
    db: std::sync::Arc<Db>,
    stop: tokio_util::sync::CancellationToken,
) {
    let mut samples = ShardSamples::default();
    tessari_wire::every(
        std::time::Duration::from_secs(tessari_constants::BALANCE_SECONDS),
        &stop,
        move |_| match db.balance_shards(&mut samples) {
            Ok(balanced) => {
                if balanced.split > 0 || balanced.merged > 0 {
                    log::info!(
                        "balanced table shards: {} split, {} pairs merged",
                        balanced.split,
                        balanced.merged
                    );
                }
                if let Some(why) = balanced.last_refusal {
                    log::warn!("a table's shards were not balanced this pass: {why}");
                }
            }
            Err(why) => log::warn!("this node cannot balance table shards: {why}"),
        },
    )
    .await;
}
