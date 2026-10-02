//! The node: listening, conversing, and pushing.
//!
//! Everything about being the server end of this protocol. The client end is
//! `client.rs`, and the two share only the frames.
//!
//! A connection is a task on the process's runtime, not a thread; the store it
//! talks to stays synchronous behind the node's bridge (ADR-0085). The
//! conversation itself is `conversation.rs`.

use std::net::{TcpListener, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tessari_constants::{GREETING_SECONDS, MAX_CONNECTIONS, MAX_STORE_CALLS};
use tessari_serve::{ACCEPT_PAUSE, Admitting, Bridge, Stopping, passes};
use tessaridb::Db;
use tessaridb::feed::Commits;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::carrier::Carrier;
use crate::conversation;
use crate::error::Result;
use crate::{frame, frame_async};

/// Names one connection across every line it produces.
///
/// Monotonic within a process and never reused, so two lines carrying the same
/// number are the same conversation. It says nothing across restarts, which is
/// all an operator following one client through a refusal needs it to say.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// The next connection's name.
pub(crate) fn next_connection() -> u64 {
    CONNECTIONS.fetch_add(1, Ordering::Relaxed)
}

/// Refuse a connection at the door, in the protocol's own words.
///
/// Best effort by construction: the client may already be gone, and a node that
/// is refusing because it is full has no capacity to spend caring. What matters
/// is that the socket closes here rather than being held.
async fn turn_away(mut stream: tokio::net::TcpStream) {
    drop(
        frame_async::write(
            &mut stream,
            frame::Kind::Refusal,
            b"this node is serving as many connections as it will",
        )
        .await,
    );
}

/// Where a connection came from, for the line that says it arrived.
fn from_where(stream: &tokio::net::TcpStream) -> String {
    stream.peer_addr().map_or_else(
        |_| "an address the socket would not give".to_owned(),
        |at| at.to_string(),
    )
}

/// A node listening for connections.
pub struct Node {
    listener: TcpListener,
    db: Arc<Db>,
    committed: Arc<Commits>,
    stopping: Arc<Stopping>,
    door: Arc<Admitting>,
    /// How many store calls this node's connections may have running at once.
    ///
    /// Sized at the door's ceiling for now, which is what one thread per
    /// connection bounded it at before; the after-measurement sets it apart.
    bridge: Arc<Bridge>,
    /// How many feed rounds may run at once, apart from statements.
    ///
    /// A feed polls the log every round whether anything happened or not, and
    /// rounds bunch: one commit wakes every feed at the same instant. Through the
    /// statements' bridge four hundred idle feeds kept four hundred blocking
    /// threads alive (measured). Bounded at the core count, a burst of rounds
    /// queues behind the cores instead — a round that finds this full waits for
    /// the next signal rather than being refused, since its subscriber was
    /// already admitted.
    rounds: Arc<Bridge>,
    /// How many busy connections may be served on a store thread at once
    /// (`hot.rs`). Beyond it a busy connection is answered from its task, as an
    /// idle one always is, so this bounds threads and never refuses anybody.
    hot: Arc<Semaphore>,
    /// What this node knows about the copies it does not hold, if anything.
    ///
    /// Held as the trait and not as the directory behind it: a node serving
    /// clients does not care *how* the answer is arrived at, only that a bounded
    /// read it cannot satisfy has somewhere to be sent. `None` on a node
    /// standing alone, which is every deployment that was never told about
    /// peers, and such a node refuses exactly as it did before.
    elsewhere: Option<Arc<dyn tessari_session::Elsewhere>>,
    /// TLS for every connection, when the node was given a certificate
    /// (ADR-0108 D4). `None` serves the protocol in the clear.
    secured: Option<tokio_rustls::TlsAcceptor>,
}

impl Node {
    /// Listen on `address`.
    ///
    /// Bound here, synchronously, so a process learns whether it has its
    /// address before it builds anything else; served by [`Node::serve`] on the
    /// runtime.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be bound.
    pub fn bind(db: Arc<Db>, address: impl ToSocketAddrs) -> Result<Self> {
        let listener = TcpListener::bind(address)?;
        // The runtime's listener requires it, and nothing here reads it blocking.
        listener.set_nonblocking(true)?;
        let committed = Arc::clone(db.commits());
        Ok(Self {
            listener,
            db,
            committed,
            stopping: Stopping::new(),
            door: Admitting::to(MAX_CONNECTIONS),
            bridge: Arc::new(Bridge::new(MAX_STORE_CALLS)),
            rounds: Arc::new(Bridge::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            )),
            hot: Arc::new(Semaphore::new(MAX_STORE_CALLS)),
            elsewhere: None,
            secured: None,
        })
    }

    /// Serve among the peers `elsewhere` knows about.
    ///
    /// Every session this node opens is given it, so a read carrying a staleness
    /// bound this node's own copy cannot satisfy is redirected to a copy that
    /// can rather than refused — C-07's *any node answers any request by serving
    /// it or by returning a redirect*.
    ///
    /// Taken as a builder rather than an argument to [`Node::bind`] because the
    /// thing that knows about peers is usually started *after* the door is open:
    /// a process binds its surfaces, then spawns the thread that greets. A
    /// required argument would force the two into an order the process does not
    /// have.
    #[must_use]
    pub fn among(mut self, elsewhere: Arc<dyn tessari_session::Elsewhere>) -> Self {
        self.elsewhere = Some(elsewhere);
        self
    }

    /// Speak TLS on every connection, and nothing else (ADR-0108 D4).
    ///
    /// There is no mixed port: a client that greets in the clear fails the
    /// handshake and never reaches the protocol, so a credential it sends is
    /// not read by this node — it has already crossed the network, which is
    /// what refusing it at the door cannot undo and does not pretend to.
    #[must_use]
    pub fn securing(mut self, settings: Arc<rustls::ServerConfig>) -> Self {
        self.secured = Some(tokio_rustls::TlsAcceptor::from(settings));
        self
    }

    /// Where it is listening, which a caller needs when it asked for port zero.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the socket cannot say.
    pub fn address(&self) -> Result<String> {
        Ok(self.listener.local_addr()?.to_string())
    }

    /// The door this node admits connections through.
    ///
    /// Exposed so a process can report how many places are taken and how many
    /// connections have been turned away — the number that says whether the
    /// ceiling is set right, and the one a refusal is worth nothing without.
    #[must_use]
    pub fn door(&self) -> Arc<Admitting> {
        Arc::clone(&self.door)
    }

    /// The bound on store calls in flight, for the process to report.
    #[must_use]
    pub fn bridge(&self) -> Arc<Bridge> {
        Arc::clone(&self.bridge)
    }

    /// What another surface needs to hold wire sessions on this node (ADR-0089).
    ///
    /// Shares this node's door, bridges, drain count and peers, so a session
    /// carried over a WebSocket is counted and bounded exactly as a TCP one is.
    #[must_use]
    pub fn carrier(&self) -> Carrier {
        Carrier {
            db: Arc::clone(&self.db),
            committed: Arc::clone(&self.committed),
            stopping: Arc::clone(&self.stopping),
            door: Arc::clone(&self.door),
            bridge: Arc::clone(&self.bridge),
            rounds: Arc::clone(&self.rounds),
            hot: Arc::clone(&self.hot),
            elsewhere: self.elsewhere.clone(),
        }
    }

    /// What this node counts as in flight, and how it is told to stop.
    ///
    /// Taken **before** [`Node::serve`], which consumes the node's borrow for
    /// as long as it runs: a caller that waited until afterwards would be asking
    /// a node that has already stopped.
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// Accept connections until `stop` is cancelled.
    ///
    /// Must be awaited inside a Tokio runtime; the node creates none of its own.
    ///
    /// # What stopping leaves running
    ///
    /// Only the accepting stops. Conversations already admitted go on — a
    /// statement in flight finishes, a feed runs until the process's stage that
    /// ends feeds — because that is what the stages wait on: the drain counts
    /// them through [`Stopping`], and aborting them here would answer the drain
    /// by killing what it was waiting for. They are detached, not leaked: their
    /// lifetime ends with the runtime's own bounded shutdown.
    ///
    /// # Errors
    ///
    /// Returns the listener's failure when accepting fails in a way that does
    /// not pass on its own, which ends the node rather than spinning (Q-834).
    pub async fn serve(&self, stop: CancellationToken) -> Result<()> {
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let carrier = self.carrier();
        let mut conversations = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(joined) = conversations.join_next(), if !conversations.is_empty() => {
                    // A connection that panics takes its own task down and
                    // nothing else: a node that one client's malformed frame
                    // could stop would be a node anybody can stop.
                    if let Err(why) = joined
                        && why.is_panic()
                    {
                        log::warn!("a connection ended in a panic: {why}");
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            if self.stopping.asked() {
                break;
            }
            let stream = match accepted {
                Ok((stream, _)) => stream,
                Err(why) if passes(&why) => {
                    log::warn!("accepting a connection failed ({why}); resting before the next");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                    continue;
                }
                Err(why) => return Err(why.into()),
            };
            let id = next_connection();
            // Before the task, because the connection is the resource being
            // bounded. A refusal costs one frame and a close — in the clear;
            // under TLS the socket is closed, since a frame before the
            // handshake is bytes the client cannot read.
            let Some(place) = self.door.admit() else {
                log::warn!(
                    "connection {id} refused from {}: {} already open",
                    from_where(&stream),
                    self.door.limit()
                );
                if self.secured.is_none() {
                    conversations.spawn(turn_away(stream));
                }
                continue;
            };
            log::info!("connection {id} accepted from {}", from_where(&stream));
            let (talk, session) = carrier.opening(id);
            let busy = self.stopping.busy();
            if let Some(acceptor) = self.secured.clone() {
                conversations.spawn(async move {
                    // Under the greeting's deadline, which is what it replaces
                    // at the door: a client that opens a socket and never
                    // finishes a handshake holds a place exactly as one that
                    // never greets would.
                    let shaken = tokio::time::timeout(
                        std::time::Duration::from_secs(GREETING_SECONDS),
                        acceptor.accept(stream),
                    )
                    .await;
                    let secured = match shaken {
                        Ok(Ok(secured)) => secured,
                        Ok(Err(why)) => {
                            log::info!("connection {id} failed its TLS handshake: {why}");
                            return;
                        }
                        Err(_) => {
                            log::info!("connection {id} did not finish its TLS handshake in time");
                            return;
                        }
                    };
                    match conversation::converse(talk, busy, place, session, secured).await {
                        Ok(()) => log::info!("connection {id} closed"),
                        Err(why) => log::info!("connection {id} ended: {why}"),
                    }
                });
                continue;
            }
            conversations.spawn(async move {
                match conversation::converse(talk, busy, place, session, stream).await {
                    Ok(()) => log::info!("connection {id} closed"),
                    // Not a warning. A client hanging up mid-frame is the
                    // ordinary end of a conversation.
                    Err(why) => log::info!("connection {id} ended: {why}"),
                }
            });
        }
        conversations.detach_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Instant;

    use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
    use tessari_types::{Epoch, Sequence};
    use tessaridb::{Db, Parameters};

    use super::Node;
    use crate::directory::Directory;
    use crate::driver::Published;
    use crate::frame;
    use crate::message::Request;
    use crate::peer::Hello;

    /// A node that cannot answer a bounded read, beside one that can.
    ///
    /// The same arrangement `talking.rs` builds for the happy path, and it is
    /// built again here rather than shared because that file cannot reach the
    /// crate-private frame vocabulary this test is written in — the whole point
    /// of the test is to speak the protocol as a client of an older build would,
    /// which no `Client` in this workspace will ever do again.
    fn a_node_that_must_redirect() -> String {
        a_node_whose_peer_claims(Roles::SERVING)
    }

    /// The same node, with the peer claiming `roles` instead.
    ///
    /// One fixture and not two, because the two redirects differ only in what
    /// sends them: the staleness axis needs a peer that merely serves, and the
    /// authority axis needs one that says it writes. Everything after that —
    /// the drained local node, the port, the thread — is the same setup, and a
    /// copy of it would be a second place for the setup to drift.
    fn a_node_whose_peer_claims(roles: Roles) -> String {
        let mut directory = Directory::new();
        directory.heard(
            "two.example:9080",
            Hello {
                node: [3; NODE_ID_LEN],
                build: NodeVersion {
                    major: 0,
                    minor: 1,
                    patch: 1,
                },
                epoch: Epoch::new(1),
                roles,
                tail: Sequence::new(4096),
                tail_leadership: Epoch::new(1),
                current_as_of: Some(std::time::Duration::from_secs(1)),
                policy: None,
                line: None,
            },
            Instant::now(),
        );

        let db = Db::in_memory().expect("an in-memory store");
        {
            // Schema first, role second: a node that may not write cannot define
            // a collection either. Holding somebody else's writes is what puts
            // this node's own copy outside every bound.
            let mut session = db.session();
            session
                .run(
                    "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                     USE DATABASE orders; DEFINE COLLECTION users;",
                )
                .expect("the schema");
            session
                .run("DEFINE NODE ROLES serving;")
                .expect("the role that stops this node writing");
        }

        let node = Node::bind(Arc::new(db), "127.0.0.1:0")
            .expect("a loopback port")
            .among(Arc::new(Published::holding(directory)));
        let address = node.address().expect("the port it took");
        drop(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
        }));
        address
    }

    /// Greet as a build of `MAJOR.minor`, and hear the node's greeting back.
    ///
    /// Written out as six bytes rather than through `frame::greet`, because that
    /// function sends whatever this build's `MINOR` happens to be — which is the
    /// value under test, so using it would make the test agree with itself.
    fn greet_as(stream: &mut TcpStream, minor: u8) {
        stream.write_all(b"TESS").expect("the magic");
        stream.write_all(&[frame::MAJOR, minor]).expect("a version");
        stream.flush().expect("the greeting");
        let mut theirs = [0_u8; 6];
        stream.read_exact(&mut theirs).expect("a greeting back");
        assert_eq!(&theirs[..4], b"TESS");
    }

    /// Ask for the bounded read, and answer with the tag that came back.
    fn tag_answering_a_bounded_read(minor: u8) -> u8 {
        tag_answering(
            a_node_that_must_redirect(),
            minor,
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users STALENESS 60s;",
        )
    }

    /// Run `script` against `address` as a build of `MAJOR.minor`, and answer
    /// with the FRAME TAG that came back — the byte itself, off the socket,
    /// rather than whatever a client would have decoded it into.
    ///
    /// That distinction is the whole point of these cases. A redirect that
    /// arrived as a refusal would still reach a caller as an error carrying an
    /// address, and every assertion made through a client would go on passing.
    fn tag_answering(address: String, minor: u8, script: &str) -> u8 {
        let mut stream = TcpStream::connect(&address).expect("the node this test started");
        greet_as(&mut stream, minor);

        let body = Request {
            script: script.to_owned(),
            credentials: None,
            parameters: Parameters::new(),
        }
        .encode();
        let length = u32::try_from(body.len()).expect("a script smaller than four gibibytes");
        stream
            .write_all(&[frame::Kind::Request.tag()])
            .expect("the tag");
        stream.write_all(&length.to_be_bytes()).expect("the length");
        stream.write_all(&body).expect("the request");
        stream.flush().expect("the request to leave");

        let mut header = [0_u8; 5];
        stream.read_exact(&mut header).expect("an answer");
        header[0]
    }

    /// Send `script` on an open connection and answer with the frame that came back.
    fn ask(stream: &mut TcpStream, script: &str) -> (u8, Vec<u8>) {
        let body = Request {
            script: script.to_owned(),
            credentials: None,
            parameters: Parameters::new(),
        }
        .encode();
        let length = u32::try_from(body.len()).expect("a short script");
        stream
            .write_all(&[frame::Kind::Request.tag()])
            .expect("the tag");
        stream.write_all(&length.to_be_bytes()).expect("the length");
        stream.write_all(&body).expect("the request");
        stream.flush().expect("the request to leave");
        let mut header = [0_u8; 5];
        stream.read_exact(&mut header).expect("an answer");
        let told = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let mut answer = vec![0_u8; usize::try_from(told).expect("a length that fits")];
        stream.read_exact(&mut answer).expect("the answer's body");
        (header[0], answer)
    }

    /// Send one request carrying `credentials`, and answer with the tag that came back.
    fn ask_as(stream: &mut TcpStream, script: &str, credentials: Option<(&str, &str)>) -> u8 {
        let body = Request {
            script: script.to_owned(),
            credentials: credentials.map(|(name, password)| (name.to_owned(), password.to_owned())),
            parameters: Parameters::new(),
        }
        .encode();
        frame::write(stream, frame::Kind::Request, &body).expect("the request");
        frame::read(stream)
            .expect("an answer")
            .expect("an answer, not a hang-up")
            .0
            .tag()
    }

    #[test]
    fn a_node_on_one_worker_answers_while_a_slow_statement_runs() {
        // S11: no store call runs on a runtime worker. One worker, and a sign-in
        // that costs a password hash on the store's side: a client arriving
        // while it runs is answered first. A statement run on the worker would
        // hold it, and nothing else on the node could even be read until the
        // hash was done.
        const PASSWORD: &str = "correct horse battery";
        let db = Arc::new(Db::in_memory().expect("an in-memory store"));
        db.session()
            .run(&format!(
                "DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
            ))
            .expect("the store closed");
        let node = Node::bind(db, "127.0.0.1:0").expect("a loopback port");
        let address = node.address().expect("the port it took");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a one-worker runtime");
        drop(std::thread::spawn(move || {
            drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
        }));

        let mut quick = TcpStream::connect(&address).expect("the node this test started");
        greet_as(&mut quick, frame::MINOR);
        let mut slow = TcpStream::connect(&address).expect("the node this test started");
        greet_as(&mut slow, frame::MINOR);
        // The sign-in is on the wire before the other client asks, and is given
        // a few milliseconds to reach its hash — which takes ~15 ms here.
        let body = Request {
            script: "INFO FOR NODE;".to_owned(),
            credentials: Some(("root".to_owned(), PASSWORD.to_owned())),
            parameters: Parameters::new(),
        }
        .encode();
        frame::write(&mut slow, frame::Kind::Request, &body).expect("the sign-in");
        let hashing = std::thread::spawn(move || {
            let answered = frame::read(&mut slow)
                .expect("an answer")
                .expect("an answer, not a hang-up");
            (answered.0.tag(), Instant::now())
        });
        std::thread::sleep(std::time::Duration::from_millis(5));
        // Anonymous on a closed store: refused by the session, with no hash.
        let tag = ask_as(&mut quick, "INFO FOR NODE;", None);
        let quick_at = Instant::now();
        assert_eq!(
            tag,
            frame::Kind::Refusal.tag(),
            "an anonymous statement was not refused"
        );
        let (tag, slow_at) = hashing.join().expect("the sign-in's thread");
        assert_eq!(
            tag,
            frame::Kind::Answer.tag(),
            "the owner's sign-in was refused"
        );
        assert!(
            quick_at < slow_at,
            "the node answered nobody while one statement hashed a password"
        );
    }

    #[test]
    fn a_statement_refused_as_busy_keeps_the_session_it_arrived_in() {
        let db = Arc::new(Db::in_memory().expect("an in-memory store"));
        let mut node = Node::bind(db, "127.0.0.1:0").expect("a loopback port");
        // One slot, so the test can take the whole bridge by holding it — the
        // resource is taken, not raced for.
        node.bridge = Arc::new(tessari_serve::Bridge::new(1));
        let bridge = node.bridge();
        let address = node.address().expect("the port it took");
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a runtime"),
        );
        let serving = Arc::clone(&runtime);
        drop(std::thread::spawn(move || {
            drop(serving.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
        }));

        let mut stream = TcpStream::connect(&address).expect("the node this test started");
        greet_as(&mut stream, frame::MINOR);
        let (tag, body) = ask(
            &mut stream,
            "DEFINE NAMESPACE shop; USE NAMESPACE shop; DEFINE DATABASE orders; \
             USE DATABASE orders; DEFINE COLLECTION items;",
        );
        assert_eq!(
            tag,
            frame::Kind::Answer.tag(),
            "the setup was refused: {}",
            String::from_utf8_lossy(&body)
        );

        // Hold the only slot with a call that waits for the test to let go.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (taken, slot_is_held) = std::sync::mpsc::channel::<()>();
        let holding = runtime.spawn(async move {
            bridge
                .call((), move |()| {
                    taken.send(()).expect("the test waiting for the slot");
                    held.recv().expect("the test letting go");
                })
                .await
        });
        slot_is_held.recv().expect("the slot taken");

        let (tag, body) = ask(&mut stream, "SELECT * FROM items;");
        assert_eq!(tag, frame::Kind::Refusal.tag(), "a full bridge answered");
        assert_eq!(
            String::from_utf8(body).expect("text"),
            super::conversation::BUSY,
            "refused, but not for being busy"
        );

        release.send(()).expect("the held call");
        drop(runtime.block_on(holding));
        let (tag, body) = ask(&mut stream, "SELECT * FROM items;");
        assert_eq!(
            tag,
            frame::Kind::Answer.tag(),
            "the refused statement lost the session: {}",
            String::from_utf8_lossy(&body)
        );
    }

    #[test]
    fn a_client_that_can_read_a_redirect_is_sent_one() {
        assert_eq!(
            tag_answering_a_bounded_read(frame::REDIRECTS),
            frame::Kind::Elsewhere.tag(),
            "the node had somewhere to send this read and refused instead"
        );
    }

    #[test]
    fn a_read_that_named_the_leader_leaves_as_a_redirect_and_not_a_refusal() {
        // The authority axis reaching the wire. It is the same frame the
        // staleness axis already sends, deliberately: one concept, one tag, one
        // arm in each transport — a second variant would be a second arm here
        // and in HTTP, where forgetting one answers a redirect as a plain
        // refusal with nothing anywhere in an error state.
        assert_eq!(
            tag_answering(
                a_node_whose_peer_claims(Roles::SERVING.and(Roles::WRITABLE)),
                frame::REDIRECTS,
                "USE NAMESPACE prod; USE DATABASE orders; \
                 SELECT * FROM users ANSWERED BY LEADER;",
            ),
            frame::Kind::Elsewhere.tag(),
            "this node knew of a peer that claims to write and refused instead"
        );
    }

    #[test]
    fn a_read_that_named_the_leader_with_no_leader_to_name_is_refused() {
        // The other half, and the one that must NOT be a redirect: every peer
        // here merely serves. A node that sent tag 13 anyway would be naming a
        // follower as the leader, which is the quiet wrong answer the whole
        // clause exists to prevent.
        assert_eq!(
            tag_answering(
                a_node_whose_peer_claims(Roles::SERVING),
                frame::REDIRECTS,
                "USE NAMESPACE prod; USE DATABASE orders; \
                 SELECT * FROM users ANSWERED BY LEADER;",
            ),
            frame::Kind::Refusal.tag(),
            "a read was sent to a peer that never claimed to write"
        );
    }

    #[test]
    fn a_client_from_before_the_redirect_existed_is_refused_rather_than_confused() {
        // The minor's whole job: what this side may SEND to an older peer. A
        // build that predates tag 13 cannot name the frame, and would have to
        // decide whether an unknown tag is corruption — which is a worse answer
        // than the refusal it has always had.
        assert_eq!(
            tag_answering_a_bounded_read(0),
            frame::Kind::Refusal.tag(),
            "a client that cannot name tag 13 was sent tag 13"
        );
    }
}
