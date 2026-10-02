//! Serving: binding the surfaces, hosting them and the node's own work on the one
//! runtime, and stopping them in order.

use crate::arguments::Serving;
use crate::greeting_round::greeting;
use crate::housekeeping::keep_house;
use crate::peers::Peering;
use crate::session::Ended;
use crate::{bootstrap, consumers, runtime, shutdown, supervise};
use tessaridb::Db;

/// How long an unseal lasts, when `--unseal-for` does not say (ADR-0092 D4).
const UNSEAL_FOR: &str = "TESSARIDB_UNSEAL_FOR";

/// How many log records each log keeps where no statement said (ADR-0094 D2).
const RETAIN_RECORDS: &str = "TESSARIDB_RETAIN_RECORDS";

/// Be the node the other half of this program connects to.
///
/// The same binary rather than a second one: what changes is where the store is,
/// and that is an argument. It serves until it is stopped, so it never returns
/// on the happy path.
pub(crate) fn serve(
    db: Db,
    serving: &Serving,
    cluster: Option<&tessari_wire::Told>,
    started: std::time::Instant,
) -> Result<Ended, String> {
    // Read before anything is bound, and before the store is touched, for the
    // reason the line below gives: an unreadable key file is a node that would
    // have come up **open**. It also costs nothing to find a path typo here
    // rather than at the first dial, which is minutes or hours later and looks
    // like a network fault.
    let cluster = match cluster {
        Some(told) => {
            Some(tessari_wire::Joining::read(told).map_err(|refused| refused.to_string())?)
        }
        None => None,
    };
    // A cluster configuration with no seed address is legal, and it is legal
    // because the catalog is the other half of the answer: a node whose
    // membership rows already name a peer has somewhere to dial and needs no
    // address on a command line — the founding node, and every node after its
    // first successful collection. `Told::from_parts` cannot ask this, because
    // it reads flags and not a store, which is the whole reason the seed used
    // to be demanded of nodes that would never read it (Q-577).
    //
    // A node with neither is the state that check was really about. It would
    // come up in a cluster it has no route into, refresh nothing, and go on
    // serving whatever it last collected — silently, because nothing is in an
    // error state while it happens. So it fails to start, here, beside the
    // credential read and for the same reason.
    if let Some(joining) = &cluster
        && joining.seeds.is_empty()
    {
        let me = db
            .store()
            .node_identity()
            .map_err(|why| format!("this node cannot say who it is: {why}"))?
            .id;
        let declared = db
            .store()
            .begin()
            .and_then(|mut transaction| tessari_storage::Catalog::new(&mut transaction).replicas())
            .map_err(|why| format!("this node cannot say who its peers are: {why}"))?;
        if !tessari_wire::names_a_peer(&declared, &me) {
            return Err(
                "this node is configured for a cluster, names no seed address, \
                        and its catalog names no peer: there is nobody it can reach. \
                        Give it --seed <node-id>@<host:port>, or declare the peer it \
                        should follow."
                    .to_owned(),
            );
        }
    }
    // Before the client doors, and before the store is touched, for the reason
    // the credential read above gives: a node that cannot take the address its
    // cluster will call it back on is a node that should fail to start, not one
    // that answers clients while silently unreachable by its peers.
    let peers = match cluster {
        Some(joining) => {
            let where_to = joining.door.clone();
            let seeds = joining.seeds.clone();
            // Taken before the bind, which consumes the first copy. Both halves
            // of the link prove the same node with the same credential.
            let dialling = joining.mine.duplicate();
            let authority = joining.authority.clone();
            let door =
                tessari_wire::Peers::bind(joining.door.as_str(), joining.mine, &joining.authority)
                    .map_err(|failure| format!("{where_to}: {failure}"))?;
            Some(Peering {
                door,
                seeds,
                dialling,
                authority,
                routing: std::sync::Arc::new(tessari_wire::Published::holding(
                    tessari_wire::Directory::new(),
                )),
            })
        }
        None => None,
    };
    // Before anything is bound. A node that came up **open** because its
    // credentials were misconfigured should never have reached the point of
    // answering on a network, so this is a failure to start rather than a
    // warning behind a listening socket.
    bootstrap::first_user(&db)?;
    let db = std::sync::Arc::new(db);
    // Every session this node opens gathers the shards of a split table it
    // lacks from their leaders (G033, ADR-0083) — on the wire and over HTTP
    // alike, which is why it is set on the store's handle and not on a surface.
    // A node with no peers has nobody to ask and refuses as it always has.
    if let Some(surface) = &peers {
        let me = db
            .store()
            .node_identity()
            .map_err(|why| format!("this node cannot say who it is: {why}"))?
            .id;
        let speaking = std::sync::Arc::downgrade(&db);
        let gathering = tessari_wire::Gathering::new(
            std::sync::Arc::clone(&db),
            me,
            (surface.dialling.duplicate(), surface.authority.clone()),
            std::sync::Arc::clone(&surface.routing),
            Box::new(move || {
                speaking
                    .upgrade()
                    .ok_or(tessari_wire::GreetingUnavailable::Stopping)
                    .and_then(|db| greeting(&db).map_err(tessari_wire::GreetingUnavailable::Store))
            }),
        );
        db.gather_through(std::sync::Arc::new(gathering));
        // Every session on every surface knows who leads and where its peers
        // are, so a read HTTP cannot answer here names — or is carried to — the
        // node that can (Q-863). Spelled with the concrete type for the unsizing.
        db.among(std::sync::Arc::<tessari_wire::Published>::clone(
            &surface.routing,
        ));
        // And every request it cannot answer — a write another node leads, a
        // read another node holds — is carried there over the peer link, under
        // an assertion signed with this node's key, for a caller who cannot
        // follow a redirect (ADR-0108 D1–D3). No password crosses.
        let speaking = std::sync::Arc::downgrade(&db);
        db.coordinate_through(std::sync::Arc::new(tessari_wire::Coordinator::new(
            &db,
            me,
            (surface.dialling.duplicate(), surface.authority.clone()),
            Box::new(move || {
                speaking
                    .upgrade()
                    .ok_or(tessari_wire::GreetingUnavailable::Stopping)
                    .and_then(|db| greeting(&db).map_err(tessari_wire::GreetingUnavailable::Store))
            }),
        )));
    }
    if let Some(folder) = &serving.backups {
        db.back_up_into(std::sync::Arc::from(folder.as_path()));
    }
    // The flag, else the variable, else the store's own ten minutes. A variable
    // that does not read as a period stops the start rather than being ignored:
    // an operator who set it believes the store closes when they said.
    let period = match serving.unseal_for {
        Some(period) => Some(period),
        None => match std::env::var(UNSEAL_FOR) {
            Ok(written) if !written.is_empty() => Some(
                crate::arguments::unseal_period(&written)
                    .map_err(|why| format!("{UNSEAL_FOR} {why}"))?,
            ),
            _ => None,
        },
    };
    if let Some(period) = period {
        db.unseal_for(period);
    }
    // The variable, else the engine's constant, which the store applies on its
    // own. Read before the housekeeping cadence starts, so the first pass prunes
    // to the window this node was started with.
    match std::env::var(RETAIN_RECORDS) {
        Ok(written) if !written.is_empty() => db.store().retain_by_default(
            crate::arguments::retained_records(&written)
                .map_err(|why| format!("{RETAIN_RECORDS} {why}"))?,
        ),
        _ => {}
    }
    // Both are bound before either serves, so an address that cannot be taken
    // is a failure to start rather than a surface that quietly went missing
    // while the other one answered.
    let wire = match &serving.wire {
        Some(address) => {
            // The directory reaches this surface's sessions through the store's
            // handle, set where the peers are (Q-863), as it reaches HTTP's.
            Some(
                tessari_wire::Node::bind(std::sync::Arc::clone(&db), address.as_str())
                    .map_err(|failure| format!("{address}: {failure}"))?,
            )
        }
        None => None,
    };
    let mut http = match &serving.http {
        Some(address) => Some(
            tessari_http::Node::bind(std::sync::Arc::clone(&db), address)
                .map_err(|failure| format!("{address}: {failure}"))?,
        ),
        None => None,
    };
    // `GET /wire` carries the wire protocol over a WebSocket (ADR-0089), so it is
    // handed the wire node's door; a process serving HTTP alone answers it 404.
    if let (Some(node), Some(http)) = (&wire, &mut http) {
        http.carrying(wire_door(node.carrier()));
    }

    // On the error stream, so a node whose output is being piped somewhere still
    // tells a person at the terminal that it came up and where. What was *bound*
    // rather than what was asked for, which is what makes `:0` usable.
    if let Some(node) = &wire {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        eprintln!("tessaridb — wire protocol on {bound}");
    }
    if let Some(node) = &http {
        eprintln!("tessaridb — http on {}", node.address());
    }
    eprintln!("tessaridb — there is no TLS, so trust the network");
    // Said only when there is something to say. Every deployment today is a
    // single node, and a line printed on every start is a line operators stop
    // reading. What was *bound* rather than what was asked for, the same as the
    // two lines above, which is what makes `:0` usable here too.
    if let Some(surface) = &peers {
        let bound = surface
            .door
            .address()
            .map_err(|failure| failure.to_string())?;
        let seeds = surface.seeds.len();
        eprintln!("tessaridb — peers on {bound}, {seeds} seed address(es) to reach the cluster");
        eprintln!("tessaridb — the peer door serves greetings and ballots, and no collection yet");
    }

    // After both surfaces are bound and before either serves, so a node that
    // could not take its address does not connect to a broker on the way to
    // failing — and so a consumer that starts is a consumer on a node that is
    // about to answer.
    //
    // Stopped at the stage that refuses new connections and joined before the
    // store is dropped — see `consumers.rs` for why a `Drop` at the end of this
    // function is neither of those moments.
    let running = consumers::start(std::sync::Arc::clone(&db));
    // The topic consumers (ADR-0087) run in every build, as a task on the
    // runtime; stage 1 stops them with the Kafka consumers.
    let (topics_stop, topics_stopped) = tokio::sync::watch::channel(false);
    let quiet = consumers::halting(&running, topics_stop);

    // What the stages will act on, taken before either surface starts serving:
    // `serve` borrows its node for as long as it runs, so a caller that asked
    // afterwards would be asking a node that had already stopped.
    // The same counters twice over, deliberately shared rather than gathered
    // separately: what a drain waits on and what a scrape reports must be one
    // set of numbers, or the two disagree in exactly the situation — a shutdown
    // — where somebody is reading both.
    let mut census = tessari_serve::Census::since(started);
    let mut surfaces = Vec::new();
    // The wire surface accepts on the runtime until this is cancelled, which is
    // the stage that refuses new connections — not the first signal, which
    // only starts the window a load balancer is given to notice.
    let wire_stops = tokio_util::sync::CancellationToken::new();
    if let Some(node) = &wire {
        census.counting("wire", node.stopping());
        let stop = wire_stops.clone();
        surfaces.push(shutdown::Surface {
            name: "the wire protocol",
            stopping: node.stopping(),
            wake: Box::new(move || stop.cancel()),
        });
    }
    // The same shape as the wire surface's: accepting stops when the stage that
    // refuses new connections cancels this.
    let http_stops = tokio_util::sync::CancellationToken::new();
    if let Some(node) = &http {
        census.counting("http", node.stopping());
        let stop = http_stops.clone();
        surfaces.push(shutdown::Surface {
            name: "http",
            stopping: node.stopping(),
            wake: Box::new(move || stop.cancel()),
        });
    }
    // The peer surface's own counts, made here rather than owned by the door,
    // because the cadences beside it live in this binary. Counted alongside
    // the others so a drain waits on it and a scrape reports it, for the reason
    // the census is shared at all: what a shutdown waits on and what a reader
    // sees must be one set of numbers. The door and the cadences stop on the
    // token, as the two client surfaces do.
    let peering = tessari_serve::Stopping::new();
    let peer_stops = tokio_util::sync::CancellationToken::new();
    if peers.is_some() {
        census.counting("peers", std::sync::Arc::clone(&peering));
        let stop = peer_stops.clone();
        surfaces.push(shutdown::Surface {
            name: "the peer door",
            stopping: std::sync::Arc::clone(&peering),
            wake: Box::new(move || stop.cancel()),
        });
    }

    // Installed once the census is complete, which is why it is a setter rather
    // than an argument to `bind`: one of the surfaces it names is this node.
    let census = std::sync::Arc::new(census);
    if let Some(node) = &mut http {
        node.watching(std::sync::Arc::clone(&census));
    }

    // The one runtime this process owns, and the token a stop arrives on — both
    // before anything serves, so a signal arriving during startup is counted
    // rather than killing the process where it stands.
    let runtime = runtime::build().map_err(|why| format!("the runtime would not start: {why}"))?;
    let asked = shutdown::listen(&runtime)
        .map_err(|why| format!("stop signals could not be registered: {why}"))?;

    // Housekeeping, and deliberately NOT one of the peer cadences.
    //
    // Trimming the log was written into the awareness round first, on the
    // argument that the round already runs and already opens the store. A live
    // run refuted it in one reading: the whole peer block is behind
    // `peers.map(…)`, so a node started without cluster credentials runs none of
    // those cadences at all — and a single node is exactly the deployment whose
    // log grows with nothing to collect it. The cadence that bounds a disk
    // cannot be one only a cluster has.
    let housekeeping = tessari_serve::Stopping::new();
    let house_stops = tokio_util::sync::CancellationToken::new();
    {
        let stop = house_stops.clone();
        surfaces.push(shutdown::Surface {
            name: "housekeeping",
            stopping: std::sync::Arc::clone(&housekeeping),
            // The cadence waits on this token between rounds, so cancelling it
            // is all a stop has to do.
            wake: Box::new(move || stop.cancel()),
        });
    }

    // Unreachable through the parser, which sets `Source::Serve` only when an
    // address was given — said here rather than assumed, because the two are
    // far enough apart to drift. Checked before anything is started, so a
    // refusal leaves nothing running behind it.
    if wire.is_none() && http.is_none() {
        return Err("--serve or --http wants an address".to_owned());
    }
    let wire = wire.map(std::sync::Arc::new);
    let http = http.map(std::sync::Arc::new);

    // Everything this process runs lives on the one runtime: the listeners,
    // the peer door, the cadences and the stages that stop them. The store is
    // reached from the runtime's blocking pool, never from a worker.
    runtime.block_on(async {
        // The node's own work — housekeeping, the peer door and the cluster
        // rounds — each under a supervisor that starts it again after a panic.
        // Joined before the store is dropped, because each holds a handle on it.
        let mut hosting = tokio::task::JoinSet::new();
        {
            let db = std::sync::Arc::clone(&db);
            let stop = house_stops.clone();
            hosting.spawn(supervise::supervised(
                "housekeeping",
                house_stops.clone(),
                move || keep_house(std::sync::Arc::clone(&db), stop.clone()),
            ));
        }

        // The declared topic consumers, joined with the rest of the node's own
        // work before the store is dropped.
        hosting.spawn(tessari_ingest::run_topic_consumers(
            db.store().clone(),
            topics_stopped,
        ));

        if let Some(surface) = peers {
            crate::peers::host(
                &mut hosting,
                std::sync::Arc::clone(&db),
                surface,
                &peer_stops,
            );
        }

        // The listeners end when the stage that refuses new connections
        // cancels their tokens; a listener that fails or panics ends the node.
        let mut listening = tokio::task::JoinSet::new();
        if let Some(node) = wire {
            let stop = wire_stops.clone();
            listening.spawn(supervise::listener("wire", async move {
                node.serve(stop).await
            }));
        }
        if let Some(node) = http {
            let stop = http_stops.clone();
            listening.spawn(supervise::listener("http", async move {
                node.serve(stop).await
            }));
        }
        // The watcher is what turns the stop token into the stages, and it runs
        // beside the listeners it stops.
        tokio::join!(shutdown::watch(&asked, &surfaces, quiet.as_ref()), async {
            while listening.join_next().await.is_some() {}
        });
        // Before the store, not after: every one of these holds an `Arc` on it,
        // so `drop(db)` below would release one handle of several and flush
        // nothing until they ended.
        while hosting.join_next().await.is_some() {}
    });
    // Before the store, not after. Stage 1 told the consumers to stop and did
    // not wait; this is the wait. Joining after `drop(db)` would flush the store
    // and release its lock while threads were still writing through it.
    consumers::stop(running);
    // Stage 4. Dropping the store is what flushes it and releases the file
    // lock, and it happens here rather than in the stages because this is what
    // owns it — the stages know about surfaces, not about a store.
    drop(db);
    runtime.shutdown_timeout(runtime::LEAVING);
    eprintln!("tessaridb — stopped");
    Ok(Ended::Fine)
}

/// The wire node's door in the shape the HTTP node takes it (ADR-0089).
///
/// Built here because this is the one place that holds both surfaces: neither
/// crate depends on the other, and a WebSocket is only ever the carrier.
fn wire_door(carrier: tessari_wire::Carrier) -> tessari_http::WireDoor {
    std::sync::Arc::new(move || {
        carrier.admit().map(|admitted| {
            let session: tessari_http::WireSession = Box::new(
                move |stream| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                    Box::pin(admitted.converse(stream))
                },
            );
            session
        })
    })
}
