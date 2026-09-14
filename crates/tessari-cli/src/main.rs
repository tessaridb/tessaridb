//! `tessaridb` — a command line for TessariDB.
//!
//! ```text
//! tessaridb                                    an in-memory store, and a prompt
//! tessaridb ./data                             a store on disk, and a prompt
//! tessaridb ./data -e 'SELECT * FROM users;'   one script, then exit
//! tessaridb ./data -f setup.tessariql          a file
//! echo 'SELECT …' | tessaridb ./data           a pipe
//! tessaridb --at 127.0.0.1:7654                a running node, and a prompt
//! tessaridb ./data --serve 0.0.0.0:7654        be that node
//! tessaridb ./data --serve :7654 --http :8000  be that node on both surfaces
//! ```
//!
//! # A path or an address, and the same prompt over either
//!
//! With a path it opens the store **in this process**, through the embedded
//! facade. With `--at` it talks to a running node over the wire protocol. Both
//! produce the same answers to the same renderer, so what is printed does not
//! depend on which one was used — see `store.rs` for why that is the shape of
//! the code rather than a claim about it.
//!
//! It is `--at` and not `--url` because this protocol has no scheme, and calling
//! an address a URL promises one. It is the same binary and not a second program
//! for the same reason `--serve` is: what changes is where the store is, and
//! that is an argument.
//!
//! Answers are printed in **TessariQL's own syntax**, so what comes out can be
//! pasted back in. JSON is what the HTTP endpoint speaks, and it had to decide
//! how seventeen types become six; a terminal is owed no such compromise.
//!
//! There is line editing and per-session history, written here rather than
//! taken as a dependency: `line.rs` says why, and `raw.rs` says what it costs.
//! Nothing is written to disk, because statements carry passwords.

mod arguments;
mod bootstrap;
mod consumers;
mod line;
mod logging;
mod raw;
mod render;
mod session;
mod shutdown;
mod store;
mod table;

use std::env;
use std::fs;
use std::io::{self, BufReader, IsTerminal, Write};
use std::process::ExitCode;

use tessaridb::Db;

use crate::arguments::{Asked, Serving, Source, credentials, parse};
use crate::session::{Ended, Mode};

fn main() -> ExitCode {
    // Before anything that could have something to report. A second logger
    // installed by an embedding caller would already have won, and that is the
    // right outcome — this one belongs to the binary.
    drop(logging::install());
    let asked = match parse(env::args().skip(1)) {
        Ok(asked) => asked,
        Err(complaint) => {
            eprintln!("{complaint}");
            return ExitCode::FAILURE;
        }
    };
    match run(asked) {
        Ok(Ended::Fine) => ExitCode::SUCCESS,
        // A refusal is an answer, and an exit code is how a shell reads one.
        Ok(Ended::Refused) => ExitCode::FAILURE,
        Err(complaint) => {
            eprintln!("tessaridb: {complaint}");
            ExitCode::FAILURE
        }
    }
}

fn run(asked: Asked) -> Result<Ended, String> {
    // Before the store is opened, because opening it is the slow part and a
    // node recovering a large log would otherwise report an uptime that began
    // after the interval a restart-detector most wants to see.
    let started = std::time::Instant::now();
    let credentials = credentials(asked.user)?;
    let parameters = asked.parameters;
    let sequence = asked.at_sequence;

    // Saying which build this is, or what the flags are, touches nothing at
    // all, so both come before even the address: they have to answer on a
    // machine with no store, no node to reach and no password to hand over.
    // Standard output and a successful exit, because both are answers rather
    // than refusals — a `--help` on standard error with a non-zero status is
    // one a pipeline cannot read and a packaging check fails on.
    match asked.source {
        Source::Version => {
            println!("tessaridb {}", tessaridb::BUILD_VERSION);
            return Ok(Ended::Fine);
        }
        Source::Help => {
            println!("{}", crate::arguments::USAGE);
            return Ok(Ended::Fine);
        }
        _ => {}
    }

    // Verifying reads a file and touches no store, so it happens before one is
    // opened — which is what makes it usable on a machine that has nothing but
    // the backup.
    if let Source::Verify(path) = &asked.source {
        return verify(path);
    }
    if let Some(address) = &asked.at {
        let mut remote = store::Remote::connect(address, credentials, parameters)?;
        let mut out = io::stdout().lock();
        return statements(&mut remote, &mut out, &asked.source, Where::Node(address));
    }

    let db = match &asked.store {
        Some(path) => Db::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
        None => Db::in_memory().map_err(|failure| failure.to_string())?,
    };

    // These are store operations rather than statements, so they never reach a
    // session at all — and an operator rehearses a restore with a command, which
    // is what "rehearsed" in the readiness checklist means.
    match &asked.source {
        Source::Backup(path) => return backup(&db, path, sequence).map(|()| Ended::Fine),
        Source::Restore(path) => return restore(&db, path, sequence).map(|()| Ended::Fine),
        Source::Health => return health(&db),
        Source::Serve => return serve(db, &asked.serving, asked.cluster.as_ref(), started),
        Source::Verify(_)
        | Source::Version
        | Source::Help
        | Source::Standard
        | Source::Inline(_)
        | Source::File(_) => {}
    }

    let mut embedded = store::Embedded::new(&db, credentials.as_ref(), parameters)?;
    let mut out = io::stdout().lock();
    let opened = asked.store.as_deref();
    statements(&mut embedded, &mut out, &asked.source, Where::Store(opened))
}

/// What the greeting says was opened.
#[derive(Debug, Clone, Copy)]
enum Where<'a> {
    /// A store in this process, or none when it is in memory.
    Store(Option<&'a std::path::Path>),
    /// A node at this address.
    Node(&'a str),
}

/// Read statements from wherever they come from and run them.
///
/// One function for both, because where the store is changes nothing about
/// where a statement ends or what a refusal does.
fn statements(
    store: &mut dyn store::Store,
    out: &mut impl Write,
    source: &Source,
    opened: Where<'_>,
) -> Result<Ended, String> {
    let ended = match source {
        Source::Inline(script) => {
            let mut input = session::Piped::new(io::Cursor::new(script.clone().into_bytes()));
            session::run(store, &mut input, out, Mode::Script)
        }
        Source::File(path) => {
            // Read rather than streamed, so a missing or unreadable file is one
            // clear failure before anything runs instead of a partial script.
            let held =
                fs::read(path).map_err(|failure| format!("{}: {failure}", path.display()))?;
            let mut input = session::Piped::new(io::Cursor::new(held));
            session::run(store, &mut input, out, Mode::Script)
        }
        Source::Backup(_)
        | Source::Restore(_)
        | Source::Verify(_)
        | Source::Version
        | Source::Help
        | Source::Health
        | Source::Serve => {
            // Resolved before this function is reached, for the embedded path,
            // and refused during parsing for a node.
            return Ok(Ended::Fine);
        }
        Source::Standard => {
            let stdin = io::stdin();
            // A prompt is for a person. Piped input gets none, so the output is
            // a script's output and not a transcript.
            let mode = if stdin.is_terminal() {
                greet(out, opened).map_err(|failure| failure.to_string())?;
                Mode::Interactive
            } else {
                Mode::Script
            };
            // A person gets the editor; a pipe gets the reader it always had.
            // `attach` answers `None` for anything that is not a terminal, so
            // the two conditions cannot come apart.
            let ended = match line::Edited::attach() {
                Some(mut edited) if mode == Mode::Interactive => {
                    session::run(store, &mut edited, out, mode)
                }
                _ => {
                    let mut input = session::Piped::new(BufReader::new(stdin.lock()));
                    session::run(store, &mut input, out, mode)
                }
            };
            if mode == Mode::Interactive && ended.is_ok() {
                // End-of-input at a prompt leaves the cursor mid-line.
                drop(writeln!(out));
            }
            ended
        }
    };
    ended.map_err(|failure| failure.to_string())
}

/// Be the node the other half of this program connects to.
///
/// The same binary rather than a second one: what changes is where the store is,
/// and that is an argument. It serves until it is stopped, so it never returns
/// on the happy path.
fn serve(
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
    // Taken here because `peers` is moved into the peer threads further down,
    // while the surface that needs it is bound in between. Both ends hold the
    // same `Published`: the dialling thread swaps a new round in, and every
    // session this node opens reads whatever the last completed round left.
    let routing = peers
        .as_ref()
        .map(|surface| std::sync::Arc::clone(&surface.routing));
    // Before anything is bound. A node that came up **open** because its
    // credentials were misconfigured should never have reached the point of
    // answering on a network, so this is a failure to start rather than a
    // warning behind a listening socket.
    bootstrap::first_user(&db)?;
    let db = std::sync::Arc::new(db);
    // Both are bound before either serves, so an address that cannot be taken
    // is a failure to start rather than a surface that quietly went missing
    // while the other one answered.
    let wire = match &serving.wire {
        Some(address) => {
            let node = tessari_wire::Node::bind(std::sync::Arc::clone(&db), address.as_str())
                .map_err(|failure| format!("{address}: {failure}"))?;
            // Only the wire surface, deliberately. `tessari-http` opens sessions
            // too and does not depend on `tessari-wire`, so giving it the
            // directory is a second wiring question rather than a line that fits
            // here — recorded rather than smuggled in.
            Some(match &routing {
                // Spelled with the concrete type because the parameter is the
                // trait: left to inference, `Arc::clone` would try to clone an
                // `Arc<dyn Elsewhere>` this line does not hold. The unsizing
                // happens at the argument, where it belongs.
                Some(known) => node.among(std::sync::Arc::<tessari_wire::Published>::clone(known)),
                None => node,
            })
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
    let running = consumers::start(&db);
    let quiet = consumers::halting(&running);

    // What the stages will act on, taken before either surface starts serving:
    // `serve` borrows its node for as long as it runs, so a caller that asked
    // afterwards would be asking a node that had already stopped.
    // The same counters twice over, deliberately shared rather than gathered
    // separately: what a drain waits on and what a scrape reports must be one
    // set of numbers, or the two disagree in exactly the situation — a shutdown
    // — where somebody is reading both.
    let mut census = tessari_serve::Census::since(started);
    let mut surfaces = Vec::new();
    if let Some(node) = &wire {
        let bound = node.address().map_err(|failure| failure.to_string())?;
        census.counting("wire", node.stopping());
        surfaces.push(shutdown::Surface {
            name: "the wire protocol",
            stopping: node.stopping(),
            // A `TcpListener` has no unblock. One throwaway connection is
            // accepted, the loop checks the flag before serving it, and both
            // end. Its failure is ignored on purpose: a listener that has
            // already stopped is the outcome this was asking for.
            wake: Box::new(move || drop(std::net::TcpStream::connect(&bound))),
        });
    }
    if let Some(node) = &http {
        let halt = node.halt();
        census.counting("http", node.stopping());
        surfaces.push(shutdown::Surface {
            name: "http",
            stopping: node.stopping(),
            wake: Box::new(move || halt.wake()),
        });
    }
    // The peer door's own flag, made here rather than owned by the door,
    // because `Peers` serves one connection per call and the loop that calls it
    // lives in this binary. Counted alongside the others so a drain waits on it
    // and a scrape reports it, for the reason the census is shared at all: what
    // a shutdown waits on and what a reader sees must be one set of numbers.
    let peering = tessari_serve::Stopping::new();
    if let Some(surface) = &peers {
        let bound = surface
            .door
            .address()
            .map_err(|failure| failure.to_string())?;
        census.counting("peers", std::sync::Arc::clone(&peering));
        surfaces.push(shutdown::Surface {
            name: "the peer door",
            stopping: std::sync::Arc::clone(&peering),
            // The same throwaway connection the wire surface uses, and for the
            // same reason: `accept` blocks and a flag does not wake it. Here the
            // connection also fails its TLS handshake, which is the outcome
            // being asked for — the loop checks the flag before waiting again.
            wake: Box::new(move || drop(std::net::TcpStream::connect(bound))),
        });
    }

    // Installed once the census is complete, which is why it is a setter rather
    // than an argument to `bind`: one of the surfaces it names is this node.
    let census = std::sync::Arc::new(census);
    if let Some(node) = &mut http {
        node.watching(std::sync::Arc::clone(&census));
    }

    // Asked for before anything serves, so a signal arriving during startup is
    // counted rather than killing the process where it stands.
    shutdown::listen();

    // Its own thread rather than an arm of the scope below, so that the peer
    // door runs whichever of the two client surfaces was asked for — including
    // neither combination the match has to spell out. It holds a handle on the
    // store, so it is joined before the store is dropped and not after.
    let peer_threads = peers.map(|surface| {
        let Peering {
            door,
            seeds,
            dialling,
            authority,
            routing,
        } = surface;
        // One copy per cadence, because the two threads that read them outlive
        // each other independently: the greeting round dials the seeds while
        // the catalog names nobody, and the collection round pulls from one of
        // them on the same condition.
        let collecting_seeds = seeds.clone();
        let dialling_seeds = seeds;
        // One voting memory, held by the door and by the campaign alike. A
        // node votes in two places — a peer's ballot arrives at the door, its
        // own arrives at home — and *a voter grants an epoch at most once* is a
        // statement about the node rather than about whichever thread happens to
        // hold the variable. Two memories would let this node grant one epoch
        // twice and hand two candidates an honest majority each.
        let deciding = std::sync::Arc::new(tessari_wire::Deciding::started());
        let answering = {
            let db = std::sync::Arc::clone(&db);
            let stopping = std::sync::Arc::clone(&peering);
            let deciding = std::sync::Arc::clone(&deciding);
            std::thread::spawn(move || greet_peers(&db, &door, &deciding, &stopping))
        };
        // A second thread, because the first one is inside `accept` for as long
        // as no peer calls: a node that only answers learns nothing about a
        // cluster that has stopped calling it. They share one flag, so the peer
        // surface stops as one thing.
        let collecting_credential = dialling.duplicate();
        let collecting_authority = authority.clone();
        // A third holder of the same `Published`. The greeting round writes it,
        // the sessions read it for staleness routing, and the collection cadence
        // now reads it to decide whose records to pull — which is the whole of
        // ADR-0065: the catalog says who may be followed, the greeting says
        // which of them is the origin right now.
        let collecting_routing = std::sync::Arc::clone(&routing);
        // A fourth holder, and the last reader the awareness round gains. A node
        // that can hear a leader does not stand against it (ADR-0066) — without
        // this the campaign cadence sees only its own lease, and a follower that
        // has none stands every second, granting an epoch to itself each time
        // and refusing the real leader's renewal for a whole TTL.
        let standing_routing = std::sync::Arc::clone(&routing);
        let standing_credential = dialling.duplicate();
        let standing_authority = authority.clone();
        let dialling = {
            let db = std::sync::Arc::clone(&db);
            let stopping = std::sync::Arc::clone(&peering);
            std::thread::spawn(move || {
                dial_peers(
                    &db,
                    &dialling,
                    &authority,
                    &dialling_seeds,
                    &routing,
                    &stopping,
                );
            })
        };
        // A third, and for the reason `driver.rs` opens with: a missed greeting
        // costs the freshness of a routing reading while a missed collection
        // costs data, and a collection blocked on a dead peer's TCP connect
        // would otherwise hold up a greeting round that has nothing to do with
        // it.
        let collecting = {
            let db = std::sync::Arc::clone(&db);
            let stopping = std::sync::Arc::clone(&peering);
            std::thread::spawn(move || {
                collect_from_upstream(
                    &db,
                    &collecting_credential,
                    &collecting_authority,
                    &collecting_seeds,
                    &collecting_routing,
                    &stopping,
                );
            })
        };
        // A fourth, and the last of the three cadences `driver.rs` names. A
        // missed renewal costs *leadership*, and it costs it on the tightest
        // deadline of the three — a fence that shuts whether or not anyone
        // noticed. Sharing a thread with a collection blocked on a dead peer's
        // TCP connect is precisely how a healthy leader would lose a lease it
        // could have renewed.
        let standing = {
            let db = std::sync::Arc::clone(&db);
            let stopping = std::sync::Arc::clone(&peering);
            std::thread::spawn(move || {
                stand_for_leadership(
                    &db,
                    &standing_credential,
                    &standing_authority,
                    &deciding,
                    &standing_routing,
                    &stopping,
                );
            })
        };
        (answering, dialling, collecting, standing)
    });

    match (wire, http) {
        // A thread for one and this thread for the other: two listeners, one
        // store, and no runtime to hold them. The watcher is a third, and it is
        // what turns a signal into the stages.
        (Some(wire), Some(http)) => std::thread::scope(|scope| {
            scope.spawn(|| shutdown::watch(&surfaces, quiet.as_ref()));
            scope.spawn(|| http.serve());
            wire.serve();
        }),
        (Some(wire), None) => std::thread::scope(|scope| {
            scope.spawn(|| shutdown::watch(&surfaces, quiet.as_ref()));
            wire.serve();
        }),
        (None, Some(http)) => std::thread::scope(|scope| {
            scope.spawn(|| shutdown::watch(&surfaces, quiet.as_ref()));
            http.serve();
        }),
        // Unreachable through the parser, which sets `Source::Serve` only when
        // an address was given — said here rather than assumed, because the two
        // are far enough apart to drift.
        (None, None) => return Err("--serve or --http wants an address".to_owned()),
    }
    // Before the store, not after, and for the reason the consumers are: this
    // thread holds an `Arc` on the store, so `drop(db)` below would release one
    // handle of two and flush nothing until it ended.
    if let Some((answering, dialling, collecting, standing)) = peer_threads {
        drop(answering.join());
        drop(dialling.join());
        drop(collecting.join());
        drop(standing.join());
    }
    // Before the store, not after. Stage 1 told the consumers to stop and did
    // not wait; this is the wait. Joining after `drop(db)` would flush the store
    // and release its lock while threads were still writing through it.
    consumers::stop(running);
    // Stage 4. Dropping the store is what flushes it and releases the file
    // lock, and it happens here rather than in the stages because this is what
    // owns it — the stages know about surfaces, not about a store.
    drop(db);
    eprintln!("tessaridb — stopped");
    Ok(Ended::Fine)
}

/// Take peers, one at a time, until the process is asked to stop.
///
/// One connection per pass, because that is what [`tessari_wire::Peers::greet`]
/// serves: a greeting, and one follow-up riding the connection it opened. A
/// thread per peer would buy concurrency this node has no use for — a cluster
/// runs three to seven voting members and a round is a handful of short
/// conversations, not a client population.
///
/// A connection that goes wrong ends that connection and nothing else. A node
/// that could be stopped by one malformed peer frame would be a node anybody
/// holding a peer credential could stop.
/// The peer surface, once the door is open.
///
/// A struct rather than a tuple because the dialling half needs two things the
/// door does not — a credential of its own and the authority to check the far
/// end against — and four positional fields threaded through three sites is
/// where a mix-up stops being visible.
struct Peering {
    /// The door peers arrive at.
    door: tessari_wire::Peers,
    /// Where to reach the cluster, until the catalog names a peer instead.
    ///
    /// Held rather than counted. It was a `usize` until W258 — enough for the
    /// startup line and nothing else — which is the whole of Q-570: the flag
    /// was parsed, counted, printed and discarded, so a node could be told
    /// where its cluster was and still had no way to reach it.
    seeds: Vec<tessari_wire::Seed>,
    /// This node's credential, for the side that calls rather than answers.
    dialling: tessari_wire::Credential,
    /// The one root every peer in this cluster is issued by.
    authority: tessari_wire::CertificateDer<'static>,
    /// What the dialling thread writes and the client surface reads.
    ///
    /// One of these, shared, and that sharing is the point of the field: a
    /// directory written by a thread nobody reads from is an accumulator, and
    /// until this wave that is exactly what it was.
    routing: std::sync::Arc<tessari_wire::Published>,
}

/// Greet every peer the catalog declares, once per awareness interval.
///
/// # Why this cadence and not a number chosen here
///
/// `tessari_session` refuses a read whose staleness bound is tighter than
/// `STALENESS_FLOOR_SECONDS`, and that floor is derived from
/// `AWARENESS_SECONDS`. The refusal is only honest if this node actually learns
/// every peer's age that often, so the period is read from the same constant the
/// floor is derived from. Two copies of it would let the promise the API makes
/// and the mechanism behind it drift apart with nothing failing.
///
/// # Why the catalog is re-read every round
///
/// A node joining the cluster is a new catalog row, and the round after it
/// appears is the one that should dial it. Reading the declarations once at
/// start would mean a node that joined had to wait for every existing node to be
/// restarted before anyone greeted it.
///
/// # What a failure does, and does not do
///
/// A peer that will not answer is left exactly as it was: its last reading stays
/// and goes on ageing, which drifts it out of tighter bounds first and looser
/// ones later, and restores it the moment it answers again. Erasing it instead
/// would put it outside *every* bound at once, so one dropped packet would take
/// a healthy node out of all routing. A round in which nobody answered is a
/// cluster in trouble rather than an operation that went wrong, so it is logged
/// and the cadence runs again.
fn dial_peers(
    db: &Db,
    mine: &tessari_wire::Credential,
    authority: &tessari_wire::CertificateDer<'static>,
    seeds: &[tessari_wire::Seed],
    published: &tessari_wire::Published,
    stopping: &tessari_serve::Stopping,
) {
    tessari_wire::every(
        std::time::Duration::from_secs(tessari_constants::AWARENESS_SECONDS),
        stopping,
        |now| {
            // Read through the pieces the facade already publishes rather than
            // through a new `Db` method: `Db::store` and `Store::begin` are both
            // public, so a `Db::declared_peers` would be a second name for a
            // capability this binary can already reach — which is the finding
            // W238 recorded when it wrote and then reverted `Db::holding`.
            let store = db.store();
            // Date this node's own tail on the same cadence, because the copy
            // age this makes measurable is only honest at the interval the
            // staleness floor is derived from. It rides this round rather than
            // the commit path deliberately: see `Store::mark_tail`.
            if let Err(why) = store.mark_tail(tessari_types::Reach::Store) {
                log::warn!("this node cannot date its own log position: {why}");
            }
            let declared = store.begin().and_then(|mut transaction| {
                tessari_storage::Catalog::new(&mut transaction).replicas()
            });
            let (me, declared) = match (store.node_identity(), declared) {
                (Ok(identity), Ok(declared)) => (identity.id, declared),
                (Err(why), _) => {
                    log::warn!("this node cannot say who it is: {why}");
                    return;
                }
                (_, Err(why)) => {
                    log::warn!("this node cannot say who its peers are: {why}");
                    return;
                }
            };
            let mut reached = 0_usize;
            published.round(|directory| {
                // The seeds INSTEAD of the catalog, and only while the catalog
                // names no peer BUT THIS NODE. A node that has just been told
                // to join holds no replica rows, so `greet_round` would dial
                // nobody and this node would never learn anything; once
                // collection brings a row naming somebody else in, the catalog
                // is the answer and a seed still being dialled would be a
                // second source of truth about who the members are — see
                // `Directory::greet_seeds`. The *but this node* is load-bearing
                // and was `is_empty` until W260: the row a cluster writes to
                // admit a newcomer describes the NEWCOMER, so the joiner's
                // first collection left it holding one row, its own, which
                // answers nothing and stopped the seed all the same.
                let greet = |endpoint: &str, node| {
                    tessari_wire::call(
                        endpoint,
                        mine.duplicate(),
                        authority,
                        node,
                        &greeting(db)?,
                        tessari_wire::Ask::Nothing,
                    )
                    .map(|(said, _)| said)
                    .map_err(|why| {
                        // Said here rather than swallowed. The rounds keep only
                        // a count, so without this the one line an operator
                        // gets for a directory that has stopped refreshing is
                        // *nobody answered* — and a directory that stops
                        // refreshing is a follower that stops knowing who to
                        // follow, whose symptom is a copy that silently never
                        // changes.
                        log::warn!("the greeting to {endpoint} did not land: {why}");
                        why.to_string()
                    })
                };
                reached = if tessari_wire::names_a_peer(&declared, &me) {
                    directory.greet_round(&declared, &me, now, greet)
                } else {
                    directory.greet_seeds(seeds, &me, now, greet)
                };
            });
            // The count and not the directory, because nothing reads the
            // directory yet — routing on it is S6.2 and is a wave of its own.
            // What this round makes observable today is that the dialling
            // happens at all and how much of the cluster answered.
            let (kind, dialled) = if tessari_wire::names_a_peer(&declared, &me) {
                ("declared peer", declared.len())
            } else {
                ("seed", seeds.len())
            };
            if reached == 0 && dialled > 0 {
                log::warn!("no {kind} answered this round; {dialled} were dialled");
            } else {
                log::info!("{reached} of {dialled} {kind}(s) answered");
            }
        },
    );
}

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
fn collect_from_upstream(
    db: &Db,
    mine: &tessari_wire::Credential,
    authority: &tessari_wire::CertificateDer<'static>,
    seeds: &[tessari_wire::Seed],
    published: &tessari_wire::Published,
    stopping: &tessari_serve::Stopping,
) {
    // One cursor per log. A node holds one log per home, and which logs it
    // should ask for is not fixed at start: a namespace arrives by collection,
    // and its own log is something to collect only once it has.
    let mut collecting = tessari_wire::Collecting::new();
    tessari_wire::every(
        std::time::Duration::from_secs(tessari_constants::COLLECTION_SECONDS),
        stopping,
        |_| {
            let store = db.store();
            let roles = match store.effective_roles() {
                Ok(roles) => roles,
                Err(why) => {
                    log::warn!("this node cannot say what it is for: {why}");
                    return;
                }
            };
            // Every declared peer, not the one row the catalog marks writable.
            // Two writable rows is the NORMAL configuration of a cluster that
            // can fail over (ADR-0063, ADR-0064), and the rule that read the
            // declaration refused exactly that shape — so the candidate set
            // comes from the catalog and the choice comes from the greetings.
            let declared = match db.store().begin().and_then(|mut transaction| {
                tessari_storage::Catalog::new(&mut transaction).replicas()
            }) {
                Ok(declared) => declared,
                Err(why) => {
                    log::warn!("this node cannot say who its peers are: {why}");
                    return;
                }
            };
            let me = match store.node_identity() {
                Ok(identity) => identity.id,
                Err(why) => {
                    log::warn!("this node cannot say who it is: {why}");
                    return;
                }
            };
            let heard = published.current();
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
                return;
            };
            let address = match endpoint.parse() {
                Ok(address) => address,
                Err(why) => {
                    log::warn!("the writable peer's endpoint {endpoint} is not an address: {why}");
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
                    return;
                }
            };
            for home in logs {
                // The seed for a log with no cursor yet: the first position
                // this node does not hold THERE. Read here rather than inside
                // the collector, which may not reach the feed.
                let seed = match store.committed_tail(home) {
                    Ok(tail) => tessari_types::Sequence::new(tail.get().saturating_add(1)),
                    Err(why) => {
                        log::warn!("this node cannot say how far {home:?} reaches: {why}");
                        continue;
                    }
                };
                let before = collecting.reached(home);
                let reached = collecting.once(home, seed, |at| collector.collect(store, home, at));
                if before == Some(reached) {
                    log::debug!(
                        "nothing collected for {home:?} from {endpoint}; still at {}",
                        reached.get()
                    );
                } else {
                    log::info!("collected {home:?} to {} from {endpoint}", reached.get());
                }
            }
        },
    );
}

/// Stand for the leadership this node is declared to hold, once per campaign
/// interval.
///
/// # What decides whether this node stands at all
///
/// [`tessari_wire::voters`] does, from the roles this node's catalog **declares**
/// and the peers it declares coordinating. The declared roles and not the
/// effective ones: the effective set drops `WRITABLE` the moment the lease is
/// spent, so a leader that lost one round would stop campaigning and could never
/// stand again — a permanent demotion that looks exactly like a correct one.
///
/// It is asked every round rather than once, so a node the operator makes
/// writable begins defending a lease without a restart, and one the operator
/// stands down stops asking for epochs at the next tick.
///
/// # Why a node that holds no leadership starts from a spent lease
///
/// [`tessari_wire::Renewing`] holds a `Leadership` and asks whether its fence is
/// near enough to need standing again. A node that holds none is, arithmetically,
/// a node holding one that has already run out — so the cursor starts one whole
/// lease in the past and the first pass stands immediately, rather than teaching
/// the driver a second state that means the same thing.
///
/// # What a lost round does, and does not do
///
/// Nothing here. `Renewing` keeps the lease it holds when a round wins nothing,
/// because a round wins nothing in the ordinary case too — a cadence that fired
/// while there was still margin asked nobody at all. Standing down on that would
/// make a healthy leader resign on a timer. What ends a leadership is the fence,
/// which the store closes on its own.
fn stand_for_leadership(
    db: &Db,
    mine: &tessari_wire::Credential,
    authority: &tessari_wire::CertificateDer<'static>,
    voter: &tessari_wire::Deciding,
    published: &tessari_wire::Published,
    stopping: &tessari_serve::Stopping,
) {
    let started = std::time::Instant::now();
    let mut renewing = tessari_wire::Renewing::holding(tessari_wire::Leadership {
        epoch: tessari_types::Epoch::ZERO,
        // A lease taken a whole TTL ago is spent, so the first pass stands. If
        // the subtraction cannot be represented — a process started before the
        // clock had a lease's worth of history behind it — the node waits out
        // one lease before its first round, which is the safe direction.
        from: started
            .checked_sub(tessari_storage::LEASE_TTL)
            .unwrap_or(started),
    });
    tessari_wire::every(
        std::time::Duration::from_secs(tessari_constants::CAMPAIGN_SECONDS),
        stopping,
        |now| {
            let store = db.store();
            // Identity first, and the catalog only once this node is known to
            // stand. Reading the roles costs a record; reading every replica the
            // catalog declares costs a transaction and a scan, and a node the
            // operator never made writable would otherwise pay for that scan
            // once a tick, for the life of the process, to reach a `return`.
            let me = match store.node_identity() {
                Ok(identity) => identity,
                Err(why) => {
                    log::warn!("this node cannot say who it is: {why}");
                    return;
                }
            };
            if !tessari_wire::stands(me.roles) {
                return;
            }
            let declared = match store.begin().and_then(|mut transaction| {
                tessari_storage::Catalog::new(&mut transaction).replicas()
            }) {
                Ok(declared) => declared,
                Err(why) => {
                    log::warn!("this node cannot say who its peers are: {why}");
                    return;
                }
            };
            let Some(voting) = tessari_wire::voters(me.roles, &declared) else {
                return;
            };
            // ADR-0066. A node that can still hear a leader does not stand
            // against it — and this is not politeness, it is what stops a
            // follower's own self-vote from refusing that leader's renewal for a
            // whole lease. The bound is the lease term, because a greeting older
            // than the leader's lease cannot testify that the leader still holds
            // it. A node that hears nothing stands, which is the condition an
            // election exists for.
            //
            // `granted_elsewhere_at(me.id)` and not the grant instant alone: a
            // candidate self-votes through this same memory, so a node reading
            // its own vote here would be silenced by the act of standing — and
            // a leader renews by standing. That is Q-602, and it made a lease
            // un-renewable.
            if tessari_wire::heard_a_leader(
                &declared,
                &published.current(),
                voter.granted_elsewhere_at(me.id),
                now,
                tessari_storage::LEASE_TTL,
            ) {
                return;
            }
            // A member whose endpoint will not parse is dropped from the set it
            // is a member of, not silently skipped inside the round: a majority
            // counted over members that cannot be asked is a majority of a
            // fiction. The operator hears about it either way.
            let mut peers = Vec::with_capacity(voting.len());
            for (node, endpoint) in &voting {
                match endpoint.parse() {
                    Ok(address) => peers.push((*node, address)),
                    Err(why) => {
                        log::warn!(
                            "the voting peer's endpoint {endpoint} is not an address: {why}"
                        );
                    }
                }
            }
            if peers.is_empty() {
                return;
            }
            let said = match greeting(db) {
                Ok(said) => said,
                Err(why) => {
                    log::warn!("this node cannot say what it holds: {why}");
                    return;
                }
            };
            // Counted here, where the decision to stand has actually been
            // taken: every gate above has passed and a round is about to open.
            // Counting at the top of the cadence would count ticks, and the
            // cadence ticks every second whether or not anything happens —
            // which is precisely the difference this counter exists to show.
            db.store().campaigned();
            let standing = tessari_wire::Standing {
                candidate: me.id,
                mine,
                authority,
                said: &said,
                peers: &peers,
                round: std::time::Duration::from_secs(tessari_constants::ROUND_SECONDS),
            };
            let before = renewing.standing();
            let held = renewing.once(me.id, now, |lease, next| {
                standing.renew(voter, lease, next, now)
            });
            if held != before {
                // Installed as it was granted, whole. The lease is dated from the
                // instant the round opened, so handing the store a span instead
                // would restart that clock here and spend the canvass out of the
                // voters' window rather than this node's.
                db.hold(held.epoch, held.lease());
                log::info!("leading at epoch {}", held.epoch.get());
                // Written here and nowhere else, because `held != before` is
                // the change: a lease is renewed every round for as long as
                // this node keeps leading, and a row per renewal would put a
                // log record on the wire every few seconds forever — one every
                // follower then pays to apply, on a log that would never
                // quiesce.
                //
                // Logged rather than propagated. The round already granted the
                // leadership and `hold` already installed it; this records that
                // grant in the log so a partitioned node can still answer who
                // leads. A store that refuses the write has not un-elected this
                // node, and treating it as fatal would let a disk hiccup
                // overturn a decision a majority took.
                if let Err(refused) = db.record_leadership(tessaridb::Reach::Store, held.epoch) {
                    log::warn!(
                        "leading at epoch {} but could not record it: {refused}",
                        held.epoch.get()
                    );
                }
            }
        },
    );
}

fn greet_peers(
    db: &Db,
    door: &tessari_wire::Peers,
    voter: &tessari_wire::Deciding,
    stopping: &tessari_serve::Stopping,
) {
    // Settled once, before the loop, and deliberately unlike the greeting below
    // it. An identity is fixed when the store is initialised, so reading it here
    // cannot go stale the way an epoch or a log tail would; a node that cannot
    // say who it is cannot admit anybody either, and the loop never opens.
    let me = match db.store().node_identity() {
        Ok(identity) => identity.id,
        Err(why) => {
            log::warn!("the peer door cannot say who this node is: {why}");
            return;
        }
    };
    while !stopping.asked() {
        // The facts are read inside the door, when a peer has arrived and
        // proved who it is — not here, before the wait. A door idle for an hour
        // used to greet with hour-old epoch, tail and copy age, which are
        // exactly the fields a router reads.
        let mine = || greeting(db).map_err(tessari_wire::Error::NothingToSay);
        // The door serves the log at last, and serves it to exactly the peers
        // this store's own catalog subscribed — `NoLog` was the honest answer
        // only while nothing could ask the catalog that question.
        match door.greet(
            mine,
            &me,
            voter,
            &tessari_wire::Serving::declared(db.store()),
        ) {
            Ok(met) => {
                log::info!(
                    "peer {} greeted at epoch {}, tail {}{}",
                    hex(&met.said.node),
                    met.said.epoch.get(),
                    met.said.tail.get(),
                    met.voted
                        .map_or(String::new(), |vote| format!(", {vote:?}")),
                );
                bind_the_greeter(db, met.said.node);
            }
            // Not a connection failure: the store itself would not answer. The
            // loop ends rather than spinning on it, and the client surfaces are
            // untouched — a node that cannot greet can still serve. It reaches
            // here rather than being read before the wait because that is the
            // whole point of reading it on arrival.
            Err(why @ tessari_wire::Error::NothingToSay(_)) => {
                log::warn!("the peer door cannot say what this node holds: {why}");
                break;
            }
            // Info and not warn. A peer hanging up, a wake-up connection, and a
            // credential this cluster does not issue are all ordinary events on
            // a door, and reporting them as problems makes the level useless for
            // finding one.
            Err(why) => log::info!("a peer connection ended: {why}"),
        }
    }
}

/// Bind the row this greeting is evidence for, when there is exactly one.
///
/// # Why this is here and not inside the door
///
/// `Peers::greet` has no store, deliberately — it settles a credential, hears a
/// greeting and answers, and a door that could also write the catalog would be a
/// transport with an opinion about membership. The rule needs two things the door
/// cannot both see, so it lives in the caller that holds both, which is the shape
/// W281 arrived at for the write fence for the same reason.
///
/// # Why a refusal is not an error here
///
/// Binding is a catalog write and therefore a log record, so only a node that may
/// write can take it: on a follower the fence refuses the commit, which is
/// correct, because membership arrives at a follower by collection and a
/// follower writing its own would be a second source of truth about who the
/// members are. The read runs first and commits nothing, so in the ordinary case
/// — every row already bound — this costs one catalog read and writes nothing at
/// all.
///
/// Logged at the level the loop already uses for ordinary peer outcomes. A
/// greeting that arrived and a row that did not need binding are both the normal
/// course of a running cluster.
fn bind_the_greeter(db: &Db, node: [u8; tessari_storage::NODE_ID_LEN]) {
    let bind = || -> Result<Option<u32>, String> {
        let mut transaction = db.store().begin().map_err(|why| why.to_string())?;
        let mut catalog = tessari_storage::Catalog::new(&mut transaction);
        let declared = catalog.replicas().map_err(|why| why.to_string())?;
        let Some(id) = tessari_storage::the_row_a_greeting_binds(&declared, &node) else {
            return Ok(None);
        };
        catalog
            .bind_replica_node(id, node)
            .map_err(|why| why.to_string())?;
        transaction.commit().map_err(|why| why.to_string())?;
        Ok(Some(id))
    };
    match bind() {
        Ok(Some(id)) => log::info!("peer {} now names replica {id}", hex(&node)),
        Ok(None) => {}
        Err(why) => log::info!("peer {} was not bound to a declared row: {why}", hex(&node)),
    }
}

/// What this node would tell a peer about itself, right now.
///
/// Built through [`tessari_wire::Hello::about`] rather than field by field, so
/// that a node cannot greet under an id, a role set or a build that disagree
/// with what its own store holds.
fn greeting(db: &Db) -> Result<tessari_wire::Hello, String> {
    let store = db.store();
    let identity = store.node_identity().map_err(|why| why.to_string())?;
    let tail = store
        .committed_tail(tessari_types::Reach::Store)
        .map_err(|why| why.to_string())?;
    let current_as_of = store.current_as_of().map_err(|why| why.to_string())?;
    // The leadership this node is actually writing under — and the trigger the
    // previous version of this line named has now fired.
    //
    // It used to be the constant `Epoch::ZERO`, which was true rather than lazy:
    // no epoch was ever allocated in this path, so every record the node held
    // belonged to the first and only leadership. A campaign now runs here, so
    // the constant would be a node telling every peer it leads under an epoch it
    // does not — and `Hello::epoch` is documented as *the leadership it believes
    // is current*, which is a claim peers route on.
    //
    // Read from the store rather than from the newest log record, which was the
    // other candidate: the record read costs a `Reach::Store` scan inside a
    // process that answers a network, and it answers a different question — what
    // leadership WROTE the last thing here, not what leadership this node holds
    // now. A follower holds records written under epochs it never led.
    //
    // `None` becomes `Epoch::ZERO`, so a node that never campaigns greets
    // byte-identically to every build before this one.
    let leading = store.leading().unwrap_or(tessari_types::Epoch::ZERO);
    // And the other epoch, which is a different fact: the leadership that WROTE
    // what this node holds, rather than the one it holds a lease under. A voter
    // ranks candidates on this pair, and ranking on `leading` instead would put
    // a follower carrying the newest records below an ex-leader carrying fewer.
    let tail_leadership = store
        .tail_leadership(tessari_types::Reach::Store)
        .map_err(|why| why.to_string())?;
    Ok(tessari_wire::Hello::about(
        &identity,
        leading,
        tail,
        tail_leadership,
        current_as_of,
    ))
}

/// A node id as it is written in a log line.
fn hex(id: &[u8]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Write the store's log to a file.
///
/// The whole store, because state is a pure function of the log — so this is a
/// complete backup and not a partial one, and restoring it is a replay.
fn backup(db: &Db, path: &std::path::Path, from: Option<u64>) -> Result<(), String> {
    let mut out = std::io::BufWriter::new(
        fs::File::create(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    // No `FROM` is the whole store, and the whole store is every log it holds.
    // A `FROM` names one sequence, which counts in one log — so it is the
    // incremental path, and a store holding several logs refuses it rather than
    // writing a file that reads as whole and is missing the rest (Q-624).
    let written = match from {
        None | Some(0 | 1) => tessari_backup::write(db.store(), &mut out),
        Some(from) => {
            let home = tessari_backup::only_log(db.store()).map_err(|why| why.to_string())?;
            tessari_backup::write_from(db.store(), &mut out, home, tessaridb::Sequence::new(from))
        }
    }
    .map_err(|failure| format!("{}: {failure}", path.display()))?;
    out.flush()
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!("{} record(s) to {}", written.records, path.display());
    for log in &written.logs {
        println!(
            "  {} sequences {}..={}",
            log_name(log.home),
            log.from,
            log.tail
        );
    }
    Ok(())
}

/// Replay a file into an empty store.
fn restore(db: &Db, path: &std::path::Path, upto: Option<u64>) -> Result<(), String> {
    let mut input = std::io::BufReader::new(
        fs::File::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    let upto = upto.map(tessaridb::Sequence::new);
    let held = tessari_backup::read_until(db.store(), &mut input, upto)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!("{} record(s) from {}", held.records, path.display());
    for log in &held.logs {
        println!("  {} through {}", log_name(log.home), log.tail);
    }
    if held.truncated {
        // Said loudly and on the error stream, because a partial restore that
        // reads as a success is how somebody learns later that the last hour is
        // gone.
        eprintln!(
            "warning: {} was cut short — {} record(s) across {} log(s) were read",
            path.display(),
            held.records,
            held.logs.len()
        );
    }
    Ok(())
}

/// Name a log the way an operator reads it.
///
/// Numbers rather than names because a backup file holds ids and nothing else:
/// resolving them would need the catalog the file is a copy of, and a restore is
/// exactly the moment that catalog may not be there yet.
fn log_name(home: tessaridb::Reach) -> String {
    match home {
        tessaridb::Reach::Store => "store".to_owned(),
        tessaridb::Reach::Namespace(namespace) => format!("namespace {}", namespace.get()),
        tessaridb::Reach::Database(namespace, database) => {
            format!("namespace {} database {}", namespace.get(), database.get())
        }
    }
}

/// Read a backup and say what it holds, applying none of it.
fn verify(path: &std::path::Path) -> Result<Ended, String> {
    let mut input = std::io::BufReader::new(
        fs::File::open(path).map_err(|failure| format!("{}: {failure}", path.display()))?,
    );
    let held = tessari_backup::verify(&mut input)
        .map_err(|failure| format!("{}: {failure}", path.display()))?;
    println!(
        "{} record(s) across {} log(s)",
        held.records,
        held.logs.len()
    );
    for log in &held.logs {
        println!(
            "  {} sequences {}..={}, good through {}",
            log_name(log.span.home),
            log.span.from,
            log.span.tail,
            log.good_through
        );
    }
    if held.truncated {
        // On the error stream and with a non-zero exit, because the whole point
        // of verifying is that somebody's script can act on the answer.
        eprintln!("warning: {} was cut short", path.display());
        for log in &held.logs {
            if log.good_through.get() < log.span.tail.get() {
                eprintln!(
                    "  {} says it holds through {} and reads through {}",
                    log_name(log.span.home),
                    log.span.tail,
                    log.good_through
                );
            }
        }
        return Ok(Ended::Refused);
    }
    Ok(Ended::Fine)
}

/// Say whether the store is well.
///
/// The same question `GET /health` answers, for an operator holding a store and
/// no server — which is exactly the situation somebody is in when they are
/// wondering whether it is still keeping their data. Exits non-zero when it is
/// not, so a cron line needs no parsing.
fn health(db: &Db) -> Result<Ended, String> {
    let held = db.store().health().map_err(|failure| failure.to_string())?;
    match held.complaint() {
        None => {
            println!("well — committed to sequence {}", held.committed);
            Ok(Ended::Fine)
        }
        Some(said) => {
            println!("unwell — {said}");
            Ok(Ended::Refused)
        }
    }
}

/// One line saying what was opened, because "which store am I in" is the first
/// thing anybody wonders at a prompt.
fn greet(out: &mut impl Write, opened: Where<'_>) -> io::Result<()> {
    match opened {
        Where::Store(Some(path)) => writeln!(out, "tessaridb — {}", path.display())?,
        Where::Store(None) => writeln!(out, "tessaridb — in memory; nothing written here is kept")?,
        Where::Node(address) => writeln!(out, "tessaridb — {address}")?,
    }
    writeln!(out, "`.help` for the little there is of it")
}
