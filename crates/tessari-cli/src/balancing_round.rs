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
        std::time::Duration::from_secs(tessari_constants::BALANCE_SECONDS),
        &stop,
        move |_| {
            match db.balance_shards(&mut samples) {
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
            }
            match db.hand_back_ranges() {
                Ok(folded) => {
                    for row in folded {
                        log::info!("handed back to the store line: the placement of {row}");
                    }
                }
                Err(why) => log::warn!("this node cannot hand ranges back: {why}"),
            }
            match db.balance_leaderships(&mut moves) {
                Ok(Some(moved)) => log::info!(
                    "balanced leaderships: moved the placement of {:?} from {} to {}",
                    moved.range,
                    moved.from,
                    moved.to
                ),
                Ok(None) => {}
                Err(why) => log::warn!("this node cannot balance leaderships: {why}"),
            }
        },
    )
    .await;
}
