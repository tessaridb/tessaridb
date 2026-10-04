use super::*;

/// Collect each placed range this node does not lead from that range's leader
/// (ADR-0082), one pass per collection tick.
///
/// The store line's pass, narrowed: the homes are the ones this node's catalog
/// says it should hold, and each is collected from the range that contains it
/// as that range's leader's log. A failure is logged and the next range still
/// runs, for the store pass's reason — one peer being unreachable is the
/// condition replication exists to survive. A range whose line this node can
/// no longer continue is repaired by a copy from its leader, as on the store
/// line; answers whether one landed, so the caller restarts its cursors too.
pub(crate) fn collect_placed_ranges(
    (db, handle): (&Db, &std::sync::Arc<Db>),
    keys: &tessari_wire::PeerKeys,
    (declared, me, heard, published): (
        &[tessari_storage::ReplicaDefinition],
        [u8; tessari_storage::NODE_ID_LEN],
        &tessari_wire::Directory,
        &std::sync::Arc<tessari_wire::Published>,
    ),
    by_leader: &mut std::collections::BTreeMap<
        [u8; tessari_storage::NODE_ID_LEN],
        tessari_wire::Collecting,
    >,
    (streams, stop, wakes): (
        &mut crate::streaming::Streams,
        &tokio_util::sync::CancellationToken,
        &std::sync::Arc<crate::peers::Wakes>,
    ),
) -> bool {
    let placed: std::collections::BTreeSet<tessari_types::Reach> =
        declared.iter().filter_map(|peer| peer.leads).collect();
    if placed.is_empty() {
        return false;
    }
    let store = db.store();
    let logs = match tessari_wire::logs_to_collect(store) {
        Ok(logs) => logs,
        Err(why) => {
            tracing::warn!(error = %why, "this node cannot say which logs it should hold");
            return false;
        }
    };
    let said = match greeting(db) {
        Ok(said) => said,
        Err(why) => {
            tracing::warn!(error = %why, "this node cannot say what it holds");
            return false;
        }
    };
    for range in placed {
        let Some((node, endpoint)) = tessari_wire::leader_of_range(range, declared, heard) else {
            continue;
        };
        if node == me {
            continue;
        }
        let Ok(address) = endpoint.parse() else {
            tracing::warn!(
                range = ?range,
                endpoint = %endpoint,
                "a range's leader has an endpoint that is not an address"
            );
            continue;
        };
        // A live stream carries this range; the round stays out of it, and a
        // stream that ended leaves the cursors to start again from the store.
        if streams.following((node, Some(range))) {
            by_leader.remove(&node);
            continue;
        }
        let collector = tessari_wire::Collector {
            keys,
            said: &said,
            peer: (node, address),
            limit: tessari_constants::COLLECTION_RECORDS,
        };
        let collecting = by_leader.entry(node).or_default();
        // One round per range, applied in its leader's commit order (ADR-0084).
        let mut asks = Vec::new();
        for home in logs.iter().copied().filter(|home| range.contains(*home)) {
            let log = match store.followed_log(home, tessari_storage::Writer::new(node)) {
                Ok(log) => log,
                Err(why) => {
                    tracing::warn!(range = ?home, error = %why, "this node cannot say which log of a range it follows");
                    continue;
                }
            };
            let seed = match store.committed_tail(log) {
                Ok(tail) => tessari_types::Sequence::new(tail.get().saturating_add(1)),
                Err(why) => {
                    tracing::warn!(range = ?home, error = %why, "this node cannot say how far a range reaches");
                    continue;
                }
            };
            asks.push((home, collecting.reached(home).unwrap_or(seed)));
        }
        let answers = collector.round(store, &asks);
        let clean = answers.iter().all(Result::is_ok);
        let below = repaired_by_a_copy(&answers);
        for ((home, at), answer) in asks.into_iter().zip(answers) {
            if let Err(why) = collecting.once(home, at, |_| answer) {
                tracing::warn!(
                    range = ?home,
                    from = %endpoint,
                    refusal = %why,
                    "collecting from the range's leader was refused"
                );
            }
        }
        // A candidate whose leader did not answer catches up from the peers
        // that hold the range instead (Q-897): a voter holding more of the line
        // refuses it a ballot for as long as it is behind, and with the leader
        // gone nothing else will ever bring it level.
        if !clean && !below && tessari_wire::stands_for(declared, &me) == Some(range) {
            for (holder, at) in holders_of(range, declared, (me, node)) {
                catch_up_from(store, (keys, &said), (holder, &at), range, &logs);
            }
        }
        if below {
            if crate::reseeding::reseed(
                db,
                keys,
                (node, &endpoint),
                &said,
                crate::reseeding::leads_a_range(declared, me),
            ) {
                // The copy stood every log where this leader's stood, and the
                // greeting the other ranges would be asked with is stale: the
                // pass ends, and the next starts every cursor from the store.
                by_leader.clear();
                return true;
            }
            continue;
        }
        if clean {
            let homes: crate::streaming::Homes = Box::new(move |db: &Db| {
                tessari_wire::logs_to_collect(db.store()).ok().map(|logs| {
                    logs.into_iter()
                        .filter(|home| range.contains(*home))
                        .collect()
                })
            });
            let heard_from = std::sync::Arc::clone(published);
            let still: crate::streaming::Still = Box::new(move |db: &Db| {
                db.store()
                    .begin()
                    .and_then(|mut transaction| {
                        tessari_storage::Catalog::new(&mut transaction).replicas()
                    })
                    .ok()
                    .and_then(|declared| {
                        tessari_wire::leader_of_range(range, &declared, &heard_from.current())
                    })
                    .map(|(leader, _)| leader)
                    == Some(node)
            });
            streams.start(
                std::sync::Arc::clone(handle),
                keys.clone(),
                ((node, Some(range)), address),
                (homes, still),
                (stop.clone(), std::sync::Arc::clone(wakes)),
            );
        }
    }
    false
}
