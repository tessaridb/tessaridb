//! Housekeeping: the node keeping its own disk in order, cluster or not.

use tessaridb::Db;

/// Keep this node's own disk in order, whether or not it has peers.
///
/// One job today: trim the log to the retained record count. It answers `None`
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
        },
    )
    .await;
}
