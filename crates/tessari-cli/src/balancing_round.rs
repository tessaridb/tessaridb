//! The balancing round: splitting and merging the shards of tables that asked
//! for it (ADR-0113 D2), folding away a placement given back to the store line
//! once its leader leads the range (ADR-0098 D3), and moving a placement when
//! the failover policy asks for balanced leaderships (ADR-0113 D3).
//!
//! A cadence of its own rather than a step of housekeeping, because a pass
//! walks shards: on a big table that is the longest thing a node does in the
//! background, and inside housekeeping it would hold up trimming the log and
//! expiring records. Only the node that may commit to the store line acts;
//! every other pass returns at once.

use tessaridb::{Db, LeadershipMoves, ShardSamples};

/// Balance shards, then leaderships, on its cadence, until `stop`.
///
/// Says at `info` every act — each is a change to a table's shard map or to a
/// placement, which an operator wants to find in the node's log beside the
/// statement in the store's — and at `warn` a refused one, which leaves a
/// table out of bounds or a node leading more than its share.
pub(crate) async fn balance(db: std::sync::Arc<Db>, stop: tokio_util::sync::CancellationToken) {
    let mut samples = ShardSamples::default();
    let mut moves = LeadershipMoves::default();
    tessari_wire::every(
        "balancing",
        std::time::Duration::from_secs(tessari_constants::BALANCE_SECONDS),
        &stop,
        move |_| {
            match db.balance_shards(&mut samples) {
                Ok(balanced) => {
                    if balanced.split > 0 || balanced.merged > 0 {
                        tracing::info!(
                            split = balanced.split,
                            merged = balanced.merged,
                            "balanced table shards"
                        );
                    }
                    if let Some(why) = balanced.last_refusal {
                        tracing::warn!(error = %why, "a table's shards were not balanced this pass");
                    }
                }
                Err(why) => tracing::warn!(error = %why, "this node cannot balance table shards"),
            }
            match db.hand_back_ranges() {
                Ok(folded) => {
                    for row in folded {
                        tracing::info!(placement = %row, "a range was handed back to the store line");
                    }
                }
                Err(why) => tracing::warn!(error = %why, "this node cannot hand ranges back"),
            }
            match db.balance_leaderships(&mut moves) {
                Ok(Some(moved)) => tracing::info!(
                    range = ?moved.range,
                    from = %moved.from,
                    to = %moved.to,
                    "balanced leaderships: a placement moved"
                ),
                Ok(None) => {}
                Err(why) => tracing::warn!(error = %why, "this node cannot balance leaderships"),
            }
            Ok(())
        },
    )
    .await;
}
