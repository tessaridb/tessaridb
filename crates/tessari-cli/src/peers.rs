//! The peer surface: the door peers arrive at and the cluster rounds beside it.

use tessaridb::Db;

use crate::collection_round::collect_from_upstream;
use crate::greeting_round::dial_peers;
use crate::leadership_round::stand_for_leadership;
use crate::peer_door::PeerDoor;
use crate::supervise;

/// The peer surface, once the door is open.
///
/// A struct rather than a tuple because four positional fields threaded through
/// three sites is where a mix-up stops being visible.
pub(crate) struct Peering {
    /// The door peers arrive at.
    pub(crate) door: tessari_wire::Peers,
    /// Where to reach the cluster, until the catalog names a peer instead.
    ///
    /// Held rather than counted. It was a `usize` until W258 — enough for the
    /// startup line and nothing else — which is the whole of Q-570: the flag
    /// was parsed, counted, printed and discarded, so a node could be told
    /// where its cluster was and still had no way to reach it.
    pub(crate) seeds: Vec<tessari_wire::Seed>,
    /// What this node presents and refuses, shared by the door and every
    /// round so a reload or a revocation reaches them all (ADR-0108 D6).
    pub(crate) keys: tessari_wire::PeerKeys,
    /// A join token to offer the seeds until a row names this node
    /// (ADR-0108 D9).
    pub(crate) join: Option<[u8; 32]>,
    /// What the greeting round writes and the client surface reads.
    ///
    /// One of these, shared, and that sharing is the point of the field: a
    /// directory written by a thread nobody reads from is an accumulator, and
    /// until this wave that is exactly what it was.
    pub(crate) routing: std::sync::Arc<tessari_wire::Published>,
}
/// What starts a cluster round before its cadence would (G053 SG2b).
///
/// A follower whose leader died learns of it from its stream in about a second,
/// and was then waiting up to a whole awareness interval to be told where the
/// leader went and a whole collection interval more to follow it there. A
/// stream that ends wakes the greeting round; a greeting round that finds a
/// different leader wakes the collection round.
#[derive(Debug, Default)]
pub(crate) struct Wakes {
    /// Wakes the greeting round.
    pub(crate) greeting: tokio::sync::Notify,
    /// Wakes the collection round.
    pub(crate) collection: tokio::sync::Notify,
}

/// Put the peer surface on the runtime: the door, and the three cluster rounds
/// `driver.rs` names, each under its own supervisor in `hosting`.
pub(crate) fn host(
    hosting: &mut tokio::task::JoinSet<()>,
    db: std::sync::Arc<Db>,
    surface: Peering,
    peer_stops: &tokio_util::sync::CancellationToken,
) {
    let Peering {
        door,
        seeds,
        keys,
        join,
        routing,
    } = surface;
    // One voting memory, held by the door and by the campaign alike. A
    // node votes in two places — a peer's ballot arrives at the door,
    // its own arrives at home — and *a voter grants an epoch at most
    // once* is a statement about the node rather than about whichever
    // task happens to hold the variable. Two memories would let this
    // node grant one epoch twice and hand two candidates an honest
    // majority each.
    let deciding = std::sync::Arc::new(tessari_wire::Deciding::started());
    let wakes = std::sync::Arc::new(Wakes::default());
    // A grant to a new leadership greets at once (Q-900): the voter is the
    // first to know the leader changed, and a follower that waited for its next
    // greeting followed nobody while the new leader's writes waited for copies.
    {
        let wakes = std::sync::Arc::clone(&wakes);
        deciding.when_granted_anew(Box::new(move || wakes.greeting.notify_one()));
    }
    // Served on the runtime, each peer in its own task (ADR-0085 §7);
    // its supervisor starts it again after a panic, as every cadence is.
    {
        let db = std::sync::Arc::clone(&db);
        let deciding = std::sync::Arc::clone(&deciding);
        let stop = peer_stops.clone();
        let door = std::sync::Arc::new(door);
        hosting.spawn(async move {
            // Settled once: an identity is fixed when the store is
            // initialised, and a node that cannot say who it is cannot
            // admit anybody either.
            let asking = std::sync::Arc::clone(&db);
            let me = match tokio::task::spawn_blocking(move || asking.store().node_identity()).await
            {
                Ok(Ok(identity)) => identity.id,
                Ok(Err(why)) => {
                    tracing::warn!(error = %why, "the peer door cannot say who this node is");
                    return;
                }
                Err(why) => {
                    tracing::warn!(error = %why, "the peer door cannot say who this node is");
                    return;
                }
            };
            let holding = std::sync::Arc::new(PeerDoor { db });
            supervise::supervised("the peer door", stop.clone(), move || {
                let door = std::sync::Arc::clone(&door);
                let stop = stop.clone();
                let deciding = std::sync::Arc::clone(&deciding);
                let holding = std::sync::Arc::clone(&holding);
                async move {
                    match door.serve(stop, me, deciding, holding).await {
                        Ok(()) => {}
                        // The store itself would not answer. The door
                        // ends rather than failing every peer in turn,
                        // and the client surfaces are untouched.
                        Err(why @ tessari_wire::Error::NothingToSay(_)) => {
                            tracing::warn!(error = %why, "the peer door cannot say what this node holds");
                        }
                        Err(why) => {
                            tracing::error!(error = %why, "the peer door failed; the node ends here");
                            crate::logging::abort();
                        }
                    }
                }
            })
            .await;
        });
    }
    // The three cadences `driver.rs` names, each a task of its own: a
    // missed greeting costs the freshness of a routing reading, a missed
    // collection costs data, and a missed renewal costs leadership on
    // the tightest deadline of the three — so a collection blocked on a
    // dead peer's TCP connect must hold up neither of the others.
    //
    // The routing directory has three readers besides the sessions: the
    // greeting round writes it, the collection round reads it to decide
    // whose records to pull (ADR-0065), and the campaign reads it so a
    // node that can hear a leader does not stand against it (ADR-0066).
    {
        let db = std::sync::Arc::clone(&db);
        let wakes = std::sync::Arc::clone(&wakes);
        let stop = peer_stops.clone();
        let (keys, seeds, routing) = (keys.clone(), seeds.clone(), std::sync::Arc::clone(&routing));
        hosting.spawn(supervise::supervised(
            "the greeting round",
            peer_stops.clone(),
            move || {
                dial_peers(
                    std::sync::Arc::clone(&db),
                    (keys.clone(), join),
                    seeds.clone(),
                    std::sync::Arc::clone(&routing),
                    std::sync::Arc::clone(&wakes),
                    stop.clone(),
                )
            },
        ));
    }
    {
        let db = std::sync::Arc::clone(&db);
        let wakes = std::sync::Arc::clone(&wakes);
        let stop = peer_stops.clone();
        let (keys, seeds, routing) = (keys.clone(), seeds.clone(), std::sync::Arc::clone(&routing));
        hosting.spawn(supervise::supervised(
            "the collection round",
            peer_stops.clone(),
            move || {
                collect_from_upstream(
                    std::sync::Arc::clone(&db),
                    keys.clone(),
                    seeds.clone(),
                    std::sync::Arc::clone(&routing),
                    std::sync::Arc::clone(&wakes),
                    stop.clone(),
                )
            },
        ));
    }
    // The fourth: finishing transactions across leaders whose coordinator
    // never came back. It asks other nodes too, so it waits on nobody's behalf.
    {
        let db = std::sync::Arc::clone(&db);
        let stop = peer_stops.clone();
        hosting.spawn(supervise::supervised(
            "the settling round",
            peer_stops.clone(),
            move || crate::settling_round::settle_across(std::sync::Arc::clone(&db), stop.clone()),
        ));
    }
    // The fifth: splitting and merging the shards of tables that asked for it
    // (ADR-0113 D2, D3). Only the store line's leader acts.
    {
        let db = std::sync::Arc::clone(&db);
        let stop = peer_stops.clone();
        hosting.spawn(supervise::supervised(
            "the balancing round",
            peer_stops.clone(),
            move || crate::balancing_round::balance(std::sync::Arc::clone(&db), stop.clone()),
        ));
    }
    {
        let db = std::sync::Arc::clone(&db);
        let stop = peer_stops.clone();
        hosting.spawn(supervise::supervised(
            "the leadership round",
            peer_stops.clone(),
            move || {
                stand_for_leadership(
                    std::sync::Arc::clone(&db),
                    keys.clone(),
                    std::sync::Arc::clone(&deciding),
                    std::sync::Arc::clone(&routing),
                    stop.clone(),
                )
            },
        ));
    }
}
