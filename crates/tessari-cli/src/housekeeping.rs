//! Housekeeping: the node keeping its own disk in order, cluster or not.

use tessaridb::Db;

/// Keep this node's own disk in order, whether or not it has peers.
///
/// Its first job: trim the log to the retained record count. It answers `None`
/// when no retention is set, which is every store until an operator sets one —
/// and that is why the silence is deliberate rather than an omission. *Nobody
/// asked for this* and *there was nothing to do* are different facts, and a
/// cadence that logged the second would bury the first under one line every ten
/// seconds forever.
///
/// When it does remove something it says so at `info`. Removing history is not a
/// thing to do quietly, and the line is the only place an operator sees that the
/// number they set is actually being enforced.
///
/// # Why a stop does not wait out the period
///
/// The cadence's wait ends the moment the token is cancelled, so a standalone
/// node — which runs no peer cadence and so was never paying a period on the
/// way out — adds nothing to a `docker stop` for a cleanup nobody is waiting on.
///
/// It runs at once rather than a period after start, so a node started with a
/// retention already set enforces it at once. A store opened after an outage
/// may have a great deal to remove, and making it wait is the one moment the
/// delay is least affordable.
pub(crate) async fn keep_house(db: std::sync::Arc<Db>, stop: tokio_util::sync::CancellationToken) {
    tessari_wire::every(
        std::time::Duration::from_secs(tessari_constants::AWARENESS_SECONDS),
        &stop,
        move |_| {
            match db.store().trim_logs() {
                Ok(Some(trimmed)) if trimmed.records > 0 => log::info!(
                    "pruned {} log record(s) across {} log(s) to the retained count",
                    trimmed.records,
                    trimmed.logs
                ),
                Ok(_) => {}
                Err(why) => log::warn!("this node cannot trim its log: {why}"),
            }
            // Removal of expired keys rides the same cadence (G035). Reads never
            // wait for it; it only returns space. A node that does not lead a
            // range is refused at commit exactly as any write would be, which is
            // the design rather than a fault, so that refusal stays quiet.
            match db.store().remove_expired() {
                Ok(lapsed) if lapsed.records > 0 || lapsed.stale > 0 => log::info!(
                    "removed {} expired record(s) in {} commit(s); {} stale expiry entr(ies)",
                    lapsed.records,
                    lapsed.batches,
                    lapsed.stale
                ),
                Ok(_) => {}
                Err(
                    why @ (tessari_storage::Error::LeaseSpent { .. }
                    | tessari_storage::Error::NoLeadershipYet
                    | tessari_storage::Error::WriteIsElsewhere { .. }),
                ) => {
                    log::debug!("expired records are removed where the range is led: {why}");
                }
                Err(why) => log::warn!("this node cannot remove expired records: {why}"),
            }
            // Transactions across leaders nobody came back to (ADR-0112 D7): an
            // overdue record this node leads is aborted, and intents whose record
            // has decided are resolved where this node leads them. Refusals
            // elsewhere are the design — another node leads — so they stay quiet.
            match db.settle_across() {
                Ok(settled) => {
                    if settled.aborted > 0 || settled.resolved > 0 {
                        log::info!(
                            "finished cross-leader transactions: {} overdue aborted, {} resolved here",
                            settled.aborted,
                            settled.resolved
                        );
                    }
                    if let Some(why) = settled.last_refusal {
                        log::debug!("a cross-leader transaction is finished elsewhere: {why}");
                    }
                }
                Err(why) => log::warn!("this node cannot finish cross-leader transactions: {why}"),
            }
            // An unseal past its period. No statement is served by that key
            // whether this runs or not — every use judges the deadline — so this
            // only stops it sitting in memory until the next use (ADR-0092 D4).
            // Said at `info`, because a store that closed itself is something
            // the operator who opened it will otherwise read as a fault.
            match db.store().vault().seal_if_due() {
                // The store's key or one vault's own (ADR-0093): either way an
                // unseal ran its period out, and the log names neither secret.
                Ok(true) => log::info!(
                    "an unseal ended and its key was dropped: an unseal lasts {}s on this node",
                    db.store().vault().period().as_secs()
                ),
                Ok(false) => {}
                Err(why) => log::warn!("this node cannot drop an expired unseal: {why}"),
            }
            // The planner's statistics, taken where an index has none or has
            // changed past the one it has (G055 W3). This node's own: each node
            // plans over its own copy, leader or not, and a statistic decides a
            // path and never an answer — so a failure here costs speed and is
            // said at `warn`, not acted on.
            match db.store().refresh_statistics() {
                Ok(taken) if taken > 0 => {
                    log::debug!("took the statistics of {taken} index(es) for the planner");
                }
                Ok(_) => {}
                Err(why) => log::warn!("this node cannot take index statistics: {why}"),
            }
            // Materialized views brought current from their sources' changes
            // (ADR-0109 D6). A view this node may not write is refused at its
            // commit before anything changes, which is the design rather than a
            // fault, so that refusal stays quiet like expiry's.
            match tessari_session::maintain_views(db.store()) {
                Ok(done) if done.views > 0 => log::debug!(
                    "applied {} change(s) to {} materialized view(s)",
                    done.changes,
                    done.views
                ),
                Ok(_) => {}
                Err(tessari_session::Error::Store(
                    tessari_storage::Error::LeaseSpent { .. }
                    | tessari_storage::Error::NoLeadershipYet
                    | tessari_storage::Error::WriteIsElsewhere { .. },
                )) => log::debug!("materialized views are kept where the range is led"),
                Err(why) => log::warn!("this node cannot keep its materialized views: {why}"),
            }
            // A series' records past its floor, removed as one range per table
            // (G044 C11). This node's own storage work: every node runs it over
            // its own copy, leader or not, because the answer already changed
            // when the floor passed.
            match db.store().expire_every_series() {
                Ok(expired) if expired.ranges > 0 => log::info!(
                    "removed aged series records as {} range(s); {} record(s) unindexed first",
                    expired.ranges,
                    expired.indexed
                ),
                Ok(_) => {}
                Err(why) => log::warn!("this node cannot remove aged series records: {why}"),
            }
        },
    )
    .await;
}
