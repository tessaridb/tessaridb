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
    mine: tessari_wire::Credential,
    authority: tessari_wire::CertificateDer<'static>,
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
    let collection = std::time::Duration::from_secs(tessari_constants::COLLECTION_SECONDS);
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
        let (db, mine, authority, seeds, published) =
            (&*db, &mine, &authority, &seeds[..], &*published);
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
        let declared =
            match db.store().begin().and_then(|mut transaction| {
                tessari_storage::Catalog::new(&mut transaction).replicas()
            }) {
                Ok(declared) => declared,
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
        collect_placed_ranges(
            (db, &handle),
            (mine, authority),
            (&declared, me, &heard, &published_handle),
            &mut by_leader,
            (&mut streams, &stopped, &wakes),
        );
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
            mine,
            authority,
            said: &said,
            peer: (node, address),
            limit: tessari_constants::COLLECTION_RECORDS,
        };
        // Every log this node should hold, store's own first, derived from
        // the catalog it has itself replayed — so the set is inside the
        // grant without the grant ever crossing the wire (Q-620, Q-621).
        let logs = match tessari_wire::logs_to_collect(store) {
            Ok(logs) => logs,
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
            let log = tessari_storage::LogId::new(home, tessari_storage::Writer::new(node));
            // The seed for a log with no cursor yet: the first position
            // this node does not hold THERE. Read here rather than inside
            // the collector, which may not reach the feed.
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
        // A log that no longer reaches back to this node's position is not
        // a refusal to retry (ADR-0094 D3): it is repaired by a copy.
        let below = answers
            .iter()
            .any(|answer| matches!(answer, Err(tessari_wire::Error::Uncollectable { .. })));
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
            let homes: crate::streaming::Homes =
                Box::new(|db: &Db| tessari_wire::logs_to_collect(db.store()).ok());
            let heard_from = std::sync::Arc::clone(&published_handle);
            let still: crate::streaming::Still =
                Box::new(move |db: &Db| store_line_upstream(db, &heard_from) == Some(node));
            streams.start(
                std::sync::Arc::clone(&handle),
                (mine.duplicate(), authority.clone()),
                ((node, None), address),
                (homes, still),
                (stopped.clone(), std::sync::Arc::clone(&wakes)),
            );
        }
        if below
            && crate::reseeding::reseed(
                db,
                (mine, authority),
                (node, &endpoint),
                &said,
                crate::reseeding::leads_a_range(&declared, me, &heard),
            )
        {
            // The copy stood each log where the leader's stood, so every
            // cursor starts again from this node's own tails.
            collecting = tessari_wire::Collecting::new();
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
/// condition replication exists to survive.
pub(crate) fn collect_placed_ranges(
    (db, handle): (&Db, &std::sync::Arc<Db>),
    (mine, authority): (
        &tessari_wire::Credential,
        &tessari_wire::CertificateDer<'static>,
    ),
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
) {
    let placed: std::collections::BTreeSet<tessari_types::Reach> =
        declared.iter().filter_map(|peer| peer.leads).collect();
    if placed.is_empty() {
        return;
    }
    let store = db.store();
    let logs = match tessari_wire::logs_to_collect(store) {
        Ok(logs) => logs,
        Err(why) => {
            log::warn!("this node cannot say which logs it should hold: {why}");
            return;
        }
    };
    let said = match greeting(db) {
        Ok(said) => said,
        Err(why) => {
            log::warn!("this node cannot say what it holds: {why}");
            return;
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
            mine,
            authority,
            said: &said,
            peer: (node, address),
            limit: tessari_constants::COLLECTION_RECORDS,
        };
        let collecting = by_leader.entry(node).or_default();
        // One round per range, applied in its leader's commit order (ADR-0084).
        let mut asks = Vec::new();
        for home in logs.iter().copied().filter(|home| range.contains(*home)) {
            let log = tessari_storage::LogId::new(home, tessari_storage::Writer::new(node));
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
        for ((home, at), answer) in asks.into_iter().zip(answers) {
            if let Err(why) = collecting.once(home, at, |_| answer) {
                log::warn!(
                    "collecting {home:?} from {endpoint}, its range's leader, was refused: {why}"
                );
            }
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
                (mine.duplicate(), authority.clone()),
                ((node, Some(range)), address),
                (homes, still),
                (stop.clone(), std::sync::Arc::clone(wakes)),
            );
        }
    }
}
