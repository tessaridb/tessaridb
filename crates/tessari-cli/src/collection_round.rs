//! The collection round: pulling the records this node follows from their writers.

use crate::greeting_round::greeting;
use tessaridb::Db;

/// Collect the records this node does not hold, once per collection interval.
///
/// # What decides whether this node collects at all
///
/// [`tessari_wire::upstream`] does, from this node's own effective roles and
/// the peer the catalog declares writable — and it is asked every round rather
/// than once, so a node told to stop writing starts following without a
/// restart, and one that has just been made writable stops following at the
/// next tick instead of applying a peer's records beside its own.
///
/// # Why the cursor is held here
///
/// [`tessari_wire::Collecting`] keeps it, and its rule is the one worth having:
/// a pass that failed leaves the cursor where it was. A cursor advanced past
/// records that were never applied skips them permanently and silently, because
/// no later pass ever asks for the gap and nothing is in an error state to say
/// so.
///
/// The starting position is this node's own committed tail plus one — the first
/// position it does not hold. It is read once, here, rather than every round:
/// after the first pass the cursor is what the collections themselves reached,
/// and re-reading the tail would hand a failed pass a fresh start it has not
/// earned.
///
/// # What a refusal does, and does not do
///
/// Nothing. A peer that has not subscribed this node, a peer whose log no
/// longer reaches back this far, a peer that is simply down — all of them leave
/// the cursor alone and are logged. The node goes on serving what it holds, and
/// its copy goes on ageing, which is exactly what a staleness bound is there to
/// notice.
pub(crate) async fn collect_from_upstream(
    db: std::sync::Arc<Db>,
    keys: tessari_wire::PeerKeys,
    seeds: Vec<tessari_wire::Seed>,
    published: std::sync::Arc<tessari_wire::Published>,
    wakes: std::sync::Arc<crate::peers::Wakes>,
    stop: tokio_util::sync::CancellationToken,
) {
    // One cursor per log. A node holds one log per home, and which logs it
    // should ask for is not fixed at start: a namespace arrives by collection,
    // and its own log is something to collect only once it has.
    let mut collecting = tessari_wire::Collecting::new();
    // One cursor set per range leader (ADR-0082): a cursor counts in one
    // writer's log, and a range whose leader changes is a different log.
    let mut by_leader: std::collections::BTreeMap<
        [u8; tessari_storage::NODE_ID_LEN],
        tessari_wire::Collecting,
    > = std::collections::BTreeMap::new();
    // The held streams this round starts once it has succeeded against a leader
    // (ADR-0106 D5). A std mutex and not a channel or a concurrent map: one
    // pass at a time touches it, never across an `.await`, and it outlives the
    // loop only so the threads can be joined off the runtime at stop.
    let streams = std::sync::Arc::new(std::sync::Mutex::new(crate::streaming::Streams::default()));
    let joined = std::sync::Arc::clone(&streams);
    let stopped = stop.clone();
    // A stream that ended leaves the round's cursors behind the store, so the
    // round starts again from the store's own tails (see `streaming.rs`).
    let mut streamed_from: Option<[u8; tessari_storage::NODE_ID_LEN]> = None;
    // The collection period the installed failover policy states, re-read every
    // pass (G053 SG2c): a pass that returns before it reaches the catalog keeps
    // the last one it read.
    let mut collection = tessari_storage::Failover::DEFAULT.collection();
    // Woken by the greeting round when the leader it points at changes, so a
    // follower whose stream ended follows the new leader as soon as one is
    // heard rather than up to a collection interval later (G053 SG2b).
    let woken = std::sync::Arc::clone(&wakes);
    tessari_wire::every_paced(&stop, &woken.collection, move |_| {
        let Ok(mut streams) = streams.lock() else {
            log::warn!("the stream registry is poisoned; collecting by rounds only");
            return collection;
        };
        let (handle, published_handle) = (
            std::sync::Arc::clone(&db),
            std::sync::Arc::clone(&published),
        );
        let (db, keys, seeds, published) = (&*db, &keys, &seeds[..], &*published);
        let store = db.store();
        let roles = match store.effective_roles() {
            Ok(roles) => roles,
            Err(why) => {
                log::warn!("this node cannot say what it is for: {why}");
                return collection;
            }
        };
        // Every declared peer, not the one row the catalog marks writable.
        // Two writable rows is the NORMAL configuration of a cluster that
        // can fail over (ADR-0063, ADR-0064), and the rule that read the
        // declaration refused exactly that shape — so the candidate set
        // comes from the catalog and the choice comes from the greetings.
        let declared = match db.store().begin().and_then(|mut transaction| {
            let catalog = tessari_storage::Catalog::new(&mut transaction);
            Ok((catalog.replicas()?, catalog.failover()?))
        }) {
            Ok((declared, policy)) => {
                collection = policy
                    .map_or(tessari_storage::Failover::DEFAULT, |definition| {
                        definition.policy
                    })
                    .collection();
                declared
            }
            Err(why) => {
                log::warn!("this node cannot say who its peers are: {why}");
                return collection;
            }
        };
        let me = match store.node_identity() {
            Ok(identity) => identity.id,
            Err(why) => {
                log::warn!("this node cannot say who it is: {why}");
                return collection;
            }
        };
        let heard = published.current();
        // ADR-0082. Before the store line's early returns below: a node that
        // may write follows nobody on the STORE line, and must still collect
        // every placed range it does not lead from that range's leader.
        if collect_placed_ranges(
            (db, &handle),
            keys,
            (&declared, me, &heard, &published_handle),
            &mut by_leader,
            (&mut streams, &stopped, &wakes),
        ) {
            // A copy from a range's leader moved every log this node holds.
            collecting = tessari_wire::Collecting::new();
        }
        // The seed INSTEAD of the catalog, and only while the catalog names
        // no peer but this node — `bootstrap_from` carries the reason it is
        // not a fallback for `upstream` answering `None`. `DEFINE REPLICA`
        // is a catalog write and therefore already a log record, so the
        // membership arrives through this very collection and the seed is
        // spent as soon as the catalog can answer. `names_a_peer` and not
        // `is_empty` because the first row that arrives is this node's own
        // and answers nothing — the bound that spent the seed on it left a
        // joiner collecting exactly once (W260).
        let origin = if tessari_wire::names_a_peer(&declared, &me) {
            tessari_wire::upstream(roles, &declared, &heard)
        } else {
            tessari_wire::bootstrap_from(roles, seeds, &heard)
        };
        let Some((node, endpoint)) = origin else {
            return collection;
        };
        // A live stream is carrying this line; the round stays out of it.
        if streams.following((node, None)) {
            streamed_from = Some(node);
            return collection;
        }
        if streamed_from.take().is_some() {
            collecting = tessari_wire::Collecting::new();
        }
        let address = match endpoint.parse() {
            Ok(address) => address,
            Err(why) => {
                log::warn!("the writable peer's endpoint {endpoint} is not an address: {why}");
                return collection;
            }
        };
        let said = match greeting(db) {
            Ok(said) => said,
            Err(why) => {
                log::warn!("this node cannot say what it holds: {why}");
                return collection;
            }
        };
        let collector = tessari_wire::Collector {
            keys,
            said: &said,
            peer: (node, address),
            limit: tessari_constants::COLLECTION_RECORDS,
        };
        // Every log this node should hold, store's own first, derived from
        // the catalog it has itself replayed — so the set is inside the
        // grant without the grant ever crossing the wire (Q-620, Q-621).
        let logs = match tessari_wire::logs_to_collect(store) {
            Ok(logs) => on_the_store_line(logs, &declared),
            Err(why) => {
                log::warn!("this node cannot say which logs it should hold: {why}");
                return collection;
            }
        };
        // Every home is asked in ONE round and applied in the writer's
        // commit order (ADR-0084): one after another, a later commit filed
        // in a coarser log was applied before an earlier one in a finer log
        // and a record both touched ended at the older value (Q-796).
        let mut asks = Vec::new();
        for home in logs {
            // The PEER's log as this node holds it, and emphatically not
            // this node's own. A log is a home AND a writer, so the two are
            // different counters over the same range — and the one the
            // collector asks a position in is the peer's, because that is
            // where the answer is filed.
            //
            // Seeding from `own_log` instead is how W382 found a cluster
            // that elected a leader and replicated nothing: a follower
            // commits its own membership before it joins, so its own log
            // stands at 1, it asked the leader for position 2 of a log it
            // held nothing of, and every pass was refused *the next record
            // must be 1, but 2 was offered* — one counter subtracted from
            // another, which is the failure `Store::committed_tail` warns
            // about in its own words.
            // The line's one log on a single-leader range, the leader's own
            // only where two may write (ADR-0107) — the log the leader serves.
            // The seed for a log with no cursor yet: the first position
            // this node does not hold THERE. Read here rather than inside
            // the collector, which may not reach the feed.
            let log = match store.followed_log(home, tessari_storage::Writer::new(node)) {
                Ok(log) => log,
                Err(why) => {
                    log::warn!("this node cannot say which log of {home:?} it follows: {why}");
                    continue;
                }
            };
            let seed = match store.committed_tail(log) {
                Ok(tail) => tessari_types::Sequence::new(tail.get().saturating_add(1)),
                Err(why) => {
                    log::warn!("this node cannot say how far {home:?} reaches: {why}");
                    continue;
                }
            };
            asks.push((home, collecting.reached(home).unwrap_or(seed)));
        }
        let answers = collector.round(store, &asks);
        let below = repaired_by_a_copy(&answers);
        // A clean round against a member of this node's own catalog is what
        // a stream starts from; a refusal or a seed is the round's alone.
        let clean = answers.iter().all(Result::is_ok) && tessari_wire::names_a_peer(&declared, &me);
        for ((home, at), answer) in asks.into_iter().zip(answers) {
            let before = collecting.reached(home);
            match collecting.once(home, at, |_| answer) {
                // A refusal, said out loud. It reaches here rather than
                // being absorbed by the cursor because *the peer turned me
                // away* and *the peer had nothing for me* leave the cursor
                // in the same place, and an operator reading only the
                // cursor cannot tell a cluster that has stopped
                // replicating from one that is level.
                Err(why) => log::warn!(
                    "collecting {home:?} from {endpoint} was refused: {why}. \
                         This node's copy of that log is not advancing."
                ),
                Ok(reached) if before == Some(reached) => log::debug!(
                    "nothing collected for {home:?} from {endpoint}; still at {}",
                    reached.get()
                ),
                Ok(reached) => {
                    log::info!("collected {home:?} to {} from {endpoint}", reached.get());
                }
            }
        }
        if clean {
            let homes: crate::streaming::Homes = Box::new(|db: &Db| {
                let declared = db
                    .store()
                    .begin()
                    .and_then(|mut transaction| {
                        tessari_storage::Catalog::new(&mut transaction).replicas()
                    })
                    .ok()?;
                tessari_wire::logs_to_collect(db.store())
                    .ok()
                    .map(|logs| on_the_store_line(logs, &declared))
            });
            let heard_from = std::sync::Arc::clone(&published_handle);
            let still: crate::streaming::Still =
                Box::new(move |db: &Db| store_line_upstream(db, &heard_from) == Some(node));
            streams.start(
                std::sync::Arc::clone(&handle),
                keys.clone(),
                ((node, None), address),
                (homes, still),
                (stopped.clone(), std::sync::Arc::clone(&wakes)),
            );
        }
        if below
            && crate::reseeding::reseed(
                db,
                keys,
                (node, &endpoint),
                &said,
                crate::reseeding::leads_a_range(&declared, me),
            )
        {
            // The copy stood each log where the leader's stood, so every
            // cursor starts again from this node's own tails — the placed
            // ranges' too, since the copy moved their logs as well.
            collecting = tessari_wire::Collecting::new();
            by_leader.clear();
        }
        collection
    })
    .await;
    // Off the runtime: each stream notices the stop within a heartbeat.
    let joining = tokio::task::spawn_blocking(move || {
        if let Ok(mut held) = joined.lock() {
            std::mem::take(&mut *held).join();
        }
    });
    if joining.await.is_err() {
        log::warn!("joining the collection streams panicked");
    }
}

/// The logs the store line carries: every log this node holds except those a
/// placement carves out (ADR-0082).
///
/// A placed range's log is written by that range's leader and collected from
/// it by [`collect_placed_ranges`]. Asked of the store line's upstream instead,
/// it is asked of a node that only follows it — or, when this node leads the
/// range, of a follower of this node — and the refusal that comes back
/// (*cannot say what precedes*, a fork) read as this node being behind, so its
/// own range was copied over from the follower and the records only it held
/// were removed (Q-884).
/// The peers other than `me` and `leader` whose subscription holds `range`,
/// with the address each is reached at — where a candidate catches up when the
/// range's leader does not answer.
fn holders_of(
    range: tessari_types::Reach,
    declared: &[tessari_storage::ReplicaDefinition],
    (me, leader): (
        [u8; tessari_storage::NODE_ID_LEN],
        [u8; tessari_storage::NODE_ID_LEN],
    ),
) -> Vec<([u8; tessari_storage::NODE_ID_LEN], std::net::SocketAddr)> {
    declared
        .iter()
        .filter(|peer| {
            peer.replicates
                .is_some_and(|over| over.contains(range) || range.contains(over))
        })
        .filter_map(|peer| Some((peer.node?, peer.endpoint.parse().ok()?)))
        .filter(|(node, _)| *node != me && *node != leader)
        .collect()
}

/// Collect `range`'s logs from `holder`, a peer holding more of its line than
/// this node, and apply what it sends.
///
/// A follower's copy of a line is the line's own records, stamped with the
/// leadership that wrote them, and the apply checks each record's predecessor
/// against this copy — so a holder whose copy parted from this one is refused
/// rather than followed. Nothing here starts a stream: the leader, once one is
/// elected, is what a range is followed from.
fn catch_up_from(
    store: &tessari_storage::Store,
    (keys, said): (&tessari_wire::PeerKeys, &tessari_wire::Hello),
    (holder, at): ([u8; tessari_storage::NODE_ID_LEN], &std::net::SocketAddr),
    range: tessari_types::Reach,
    logs: &[tessari_types::Reach],
) {
    let collector = tessari_wire::Collector {
        keys,
        said,
        peer: (holder, *at),
        limit: tessari_constants::COLLECTION_RECORDS,
    };
    let asks: Vec<_> = logs
        .iter()
        .copied()
        .filter(|home| range.contains(*home))
        .filter_map(|home| {
            let log = store
                .followed_log(home, tessari_storage::Writer::new(holder))
                .ok()?;
            let tail = store.committed_tail(log).ok()?;
            Some((
                home,
                tessari_types::Sequence::new(tail.get().saturating_add(1)),
            ))
        })
        .collect();
    for ((home, from), answer) in asks.iter().zip(collector.round(store, &asks)) {
        match answer {
            Ok(reached) if reached.get() >= from.get() => {
                log::info!(
                    "caught {home:?} up to {} from {at}, which holds it",
                    reached.get()
                );
            }
            Ok(_) => {}
            Err(why) => log::debug!("catching {home:?} up from {at} was refused: {why}"),
        }
    }
}

/// Whether a round met a log it can only be repaired on by a copy.
///
/// A log that no longer reaches back to this node's position is not a refusal
/// to retry (ADR-0094 D3): it is repaired by a copy — and so is a copy that
/// forked from the line (ADR-0107, Q-879 H2), which meets the same record on
/// every pass. The same on the store line and on a placed range's line: a
/// former leader of a range, moved away while its last writes were held by
/// nobody else, holds a record its successor's line does not (Q-884).
fn repaired_by_a_copy<T>(answers: &[Result<T, tessari_wire::Error>]) -> bool {
    answers.iter().any(|answer| {
        matches!(
            answer,
            Err(tessari_wire::Error::Uncollectable { .. } | tessari_wire::Error::Forked { .. })
        )
    })
}

fn on_the_store_line(
    logs: Vec<tessari_types::Reach>,
    declared: &[tessari_storage::ReplicaDefinition],
) -> Vec<tessari_types::Reach> {
    let placed: std::collections::BTreeSet<tessari_types::Reach> =
        declared.iter().filter_map(|peer| peer.leads).collect();
    logs.into_iter()
        .filter(|home| tessari_storage::governing(&placed, *home) == tessari_types::Reach::Store)
        .collect()
}

/// The node the store line follows right now, asked the way the round asks it.
fn store_line_upstream(
    db: &Db,
    published: &tessari_wire::Published,
) -> Option<[u8; tessari_storage::NODE_ID_LEN]> {
    let store = db.store();
    let roles = store.effective_roles().ok()?;
    let declared = store
        .begin()
        .and_then(|mut transaction| tessari_storage::Catalog::new(&mut transaction).replicas())
        .ok()?;
    tessari_wire::upstream(roles, &declared, &published.current()).map(|(node, _)| node)
}

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
            log::warn!("this node cannot say which logs it should hold: {why}");
            return false;
        }
    };
    let said = match greeting(db) {
        Ok(said) => said,
        Err(why) => {
            log::warn!("this node cannot say what it holds: {why}");
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
            log::warn!(
                "the leader of {range:?} has an endpoint that is not an address: {endpoint}"
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
                    log::warn!("this node cannot say which log of {home:?} it follows: {why}");
                    continue;
                }
            };
            let seed = match store.committed_tail(log) {
                Ok(tail) => tessari_types::Sequence::new(tail.get().saturating_add(1)),
                Err(why) => {
                    log::warn!("this node cannot say how far {home:?} reaches: {why}");
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
                log::warn!(
                    "collecting {home:?} from {endpoint}, its range's leader, was refused: {why}"
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

#[cfg(test)]
mod tests {
    use tessari_storage::ReplicaDefinition;
    use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};

    use super::on_the_store_line;

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
            releasing: false,
        }
    }

    fn shard(id: u32) -> Reach {
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            ShardId::new(id),
        )
    }

    #[test]
    fn a_candidate_catches_up_from_the_holders_of_its_range_and_no_one_else() {
        let (me, leader, holder, stranger) = ([1; 16], [2; 16], [3; 16], [4; 16]);
        let holding = |node, over| ReplicaDefinition {
            replicates: over,
            ..row(Some(node), None)
        };
        let declared = [
            holding(me, Some(Reach::Store)),
            holding(leader, Some(shard(2))),
            holding(holder, Some(Reach::Store)),
            holding(stranger, Some(shard(1))),
            holding([5; 16], None),
        ];
        let found: Vec<_> = super::holders_of(shard(2), &declared, (me, leader))
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        assert_eq!(found, vec![holder]);
    }

    #[test]
    fn the_store_line_does_not_carry_a_placed_range() {
        let declared = [
            row(None, Some(shard(1))),
            row(None, Some(shard(2))),
            row(None, None),
        ];
        let held = vec![
            Reach::Store,
            Reach::Namespace(NamespaceId::new(1)),
            Reach::Database(NamespaceId::new(1), DatabaseId::new(1)),
            shard(1),
            shard(2),
            shard(3),
        ];
        assert_eq!(
            on_the_store_line(held.clone(), &declared),
            vec![held[0], held[1], held[2], shard(3)],
            "a shard a placement carves out is collected from its own leader"
        );
        assert_eq!(on_the_store_line(held.clone(), &[]), held);
    }
}
