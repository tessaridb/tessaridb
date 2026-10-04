//! The settling round: finishing transactions across leaders that nobody came
//! back to (ADR-0112 D7).
//!
//! A cadence of its own rather than a step of housekeeping, for the reason the
//! three cluster rounds are three: it asks other nodes, and a leader that hangs
//! holds a pass for as long as the peer link's patience. Inside housekeeping
//! that wait would hold up trimming the log, expiring records and sealing the
//! vault — none of which needs anybody else. Here it holds up only itself, and
//! [`Db::settle_across`] asks a leader that did not answer once per pass.

use tessaridb::Db;

/// Finish this node's stranded cross-leader transactions on the awareness
/// cadence, until `stop`.
///
/// Says at `info` what it finished, and at `debug` the last refusal of a pass:
/// a refusal here is mostly the design — another node leads the intents' home,
/// or the record's leader is unreachable for now — and repeating it at `warn`
/// every second would bury the one line that matters.
pub(crate) async fn settle_across(
    db: std::sync::Arc<Db>,
    stop: tokio_util::sync::CancellationToken,
) {
    tessari_wire::every(
        std::time::Duration::from_secs(tessari_constants::AWARENESS_SECONDS),
        &stop,
        move |_| match db.settle_across() {
            Ok(settled) => {
                if settled.aborted > 0 || settled.committed > 0 || settled.resolved > 0 {
                    tracing::info!(
                        aborted = settled.aborted,
                        committed = settled.committed,
                        resolved = settled.resolved,
                        "finished cross-leader transactions"
                    );
                }
                // Every decided transaction ends here, so it is routine and
                // said at `debug`: a count at `info` would repeat each second.
                if settled.forgotten > 0 {
                    tracing::debug!(
                        forgotten = settled.forgotten,
                        "forgot decided cross-leader records"
                    );
                }
                if let Some(why) = settled.last_refusal {
                    tracing::debug!(
                        unreachable = settled.unreachable,
                        reason = %why,
                        "a cross-leader transaction is finished elsewhere or later"
                    );
                }
            }
            Err(why) => {
                tracing::warn!(error = %why, "this node cannot finish cross-leader transactions")
            }
        },
    )
    .await;
}
