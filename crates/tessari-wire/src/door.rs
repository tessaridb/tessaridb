//! The peer door, served on the process's runtime.
//!
//! # Why the door stopped being a loop around [`Peers::greet`]
//!
//! `greet` accepts one connection and serves it to the end, so a node that
//! called it in a loop served its peers one at a time. A caller that opened a
//! socket and then said nothing — a stranger, or a peer that died mid-handshake
//! — held the door for [`GREETING_SECONDS`] on every read, and every ballot,
//! greeting and collection from every other peer waited behind it. A lease
//! renewal is one of those, and its deadline does not wait.
//!
//! Here each connection is its own task. The handshake, the greeting and the
//! one follow-up are exactly `greet`'s, in the same order and under the same
//! bound per step; the decision about the follow-up is the same function
//! ([`answering`]); what changes is only that nobody waits in line.
//!
//! # What the concurrency does not change
//!
//! Two ballots can now be decided at the same moment. The rule that matters —
//! a voter grants an epoch at most once — was never held by the door being
//! serial: [`Deciding`] makes each decision under its own lock, and the door
//! and the node's own campaign already reached it from two threads.
//!
//! # Where the store is touched
//!
//! Only through the bridge: what this node says it holds is read when a peer
//! has arrived and proved itself, as `greet` reads it, and the answer to a
//! collection or a gather reads the log. Both block, so both run off the
//! runtime's workers and count against the bridge's bound.

use std::sync::Arc;

use tessari_constants::{GREETING_SECONDS, PEER_CONNECTIONS, STREAM_HEARTBEAT_MILLIS};
use tessari_encoding::NODE_ID_LEN;
use tessari_serve::{ACCEPT_PAUSE, Bridge, Bridged, passes};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::assertion::{Assertion, Replays, now_ms};
use crate::collection::{Origin, StreamAsk, stream_answer};
use crate::coordination::{Coordinate, encode_answer};
use crate::credential;
use crate::error::{Error, Result};
use crate::frame_async;
use crate::grant::Deciding;
use crate::link::{Met, Peers, answering, greeting};
use crate::peer::{Hello, PeerFrame, admit};

/// What the door needs from the node it belongs to.
///
/// The log it answers collections and gathers from, what the node holds right
/// now, and somewhere to report a peer it served. Every method blocks on the
/// store and is called through the bridge, never on a runtime worker.
pub trait Holding: Origin + Send + Sync + 'static {
    /// What this node holds, read after a peer has arrived and proved itself.
    ///
    /// # Errors
    ///
    /// [`Error::NothingToSay`] when the store cannot answer, which ends the
    /// door rather than every connection failing the same way in turn.
    fn hello(&self) -> Result<Hello>;

    /// A peer was served to the end; record what it said and how it was voted.
    fn met(&self, met: &Met);

    /// A wake-up that moves whenever this node lands a commit, so a held
    /// stream sends a record the moment it exists (ADR-0106 D5) instead of on
    /// the follower's next clock tick.
    ///
    /// No default: a door that never woke would still answer every stream —
    /// at its heartbeat — and the replication lag it was built to remove would
    /// come back with nothing in an error state.
    fn commits(&self) -> tokio::sync::watch::Receiver<u64>;

    /// Answer a request `from` carried here for a caller it verified, under an
    /// assertion the door has already believed (ADR-0108 D1–D3).
    ///
    /// # Errors
    ///
    /// The reason this node will not act for that caller, in words the asking
    /// node passes on.
    fn coordinated(
        &self,
        from: [u8; NODE_ID_LEN],
        assertion: &Assertion,
        asked: &Coordinate,
    ) -> std::result::Result<tessaridb::Coordinated, String>;
}

/// How one served connection ended, for the loop that decides what next.
enum Ended {
    /// Served, or ended the way a connection ordinarily ends.
    Served,
    /// This node could not say what it holds, so the door stops.
    NothingToSay(Error),
}

impl Peers {
    /// Serve peers until `stop` is cancelled, each connection in its own task.
    ///
    /// Must be awaited inside a Tokio runtime; the door creates none. May be
    /// called again after it returns, which is what lets a supervisor start
    /// it again.
    ///
    /// On stop the door accepts nobody new and waits for the connections it
    /// took, each of which is bounded by [`GREETING_SECONDS`] per step.
    ///
    /// # Errors
    ///
    /// The listener's failure when accepting fails in a way that does not pass
    /// on its own (Q-834), and [`Error::NothingToSay`] when this node could not
    /// state what it holds — the door ends then rather than failing every peer.
    pub async fn serve<H: Holding>(
        &self,
        stop: CancellationToken,
        me: [u8; NODE_ID_LEN],
        voter: Arc<Deciding>,
        holding: Arc<H>,
    ) -> Result<()> {
        let listener = self.listener.try_clone()?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let acceptor = TlsAcceptor::from(Arc::clone(&self.settings));
        let places = Arc::new(Semaphore::new(PEER_CONNECTIONS));
        let bridge = Arc::new(Bridge::new(PEER_CONNECTIONS));
        let replays = Arc::new(Replays::default());
        let mut connections = JoinSet::new();
        let mut outcome = Ok(());
        loop {
            let accepted = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    match joined {
                        Ok(Ended::Served) => {}
                        Ok(Ended::NothingToSay(why)) => {
                            outcome = Err(why);
                            break;
                        }
                        // A connection that panics takes its own task down
                        // and nothing else.
                        Err(why) => log::warn!("a peer connection ended in a panic: {why}"),
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            let socket = match accepted {
                Ok((socket, _)) => socket,
                Err(why) if passes(&why) => {
                    log::warn!("accepting a peer failed ({why}); resting before the next");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                    continue;
                }
                Err(why) => return Err(why.into()),
            };
            // Before the task, because the connection is what is bounded.
            let Ok(place) = Arc::clone(&places).try_acquire_owned() else {
                log::warn!(
                    "a peer connection was closed unanswered: {PEER_CONNECTIONS} already open"
                );
                continue;
            };
            let served = Connection {
                stop: stop.clone(),
                acceptor: acceptor.clone(),
                me,
                voter: Arc::clone(&voter),
                holding: Arc::clone(&holding),
                bridge: Arc::clone(&bridge),
                replays: Arc::clone(&replays),
            };
            connections.spawn(async move {
                let ended = served.serve(socket).await;
                drop(place);
                ended
            });
        }
        // The connections already taken finish, under one deadline for the
        // lot: each is bounded per step, but a collection is many steps, and a
        // stop that waited on it without a bound would not be a stop.
        let drained =
            tokio::time::timeout(std::time::Duration::from_secs(GREETING_SECONDS), async {
                while connections.join_next().await.is_some() {}
            })
            .await;
        if drained.is_err() {
            log::warn!("the peer door stopped with connections still open; they are cut");
            connections.abort_all();
        }
        outcome
    }
}

/// Everything one connection needs, owned so the task can hold it.
struct Connection<H> {
    /// The door's own stop, so a held stream ends with the door rather than
    /// being cut at the drain deadline.
    stop: CancellationToken,
    acceptor: TlsAcceptor,
    me: [u8; NODE_ID_LEN],
    voter: Arc<Deciding>,
    holding: Arc<H>,
    bridge: Arc<Bridge>,
    /// The nonces of the assertions this door believed, so none is believed
    /// twice (ADR-0108 D3).
    replays: Arc<Replays>,
}

impl<H: Holding> Connection<H> {
    /// Serve one peer, and say how it ended.
    async fn serve(self, socket: tokio::net::TcpStream) -> Ended {
        match self.exchange(socket).await {
            // A stream recorded its peer when it opened (see `stream`).
            Ok(None) => Ended::Served,
            Ok(Some(met)) => {
                let holding = Arc::clone(&self.holding);
                if let Bridged::Busy(()) | Bridged::Panicked =
                    self.bridge.call((), move |()| holding.met(&met)).await
                {
                    log::warn!("a peer was served but could not be recorded");
                }
                Ended::Served
            }
            Err(why @ Error::NothingToSay(_)) => Ended::NothingToSay(why),
            // Info and not warn: a peer hanging up and a credential this
            // cluster does not issue are ordinary events on a door.
            Err(why) => {
                log::info!("a peer connection ended: {why}");
                Ended::Served
            }
        }
    }

    /// The handshake, the greetings and the one follow-up — `greet`'s order.
    async fn exchange(&self, socket: tokio::net::TcpStream) -> Result<Option<Met>> {
        let mut link = bounded(self.acceptor.accept(socket))
            .await?
            .map_err(|why| Error::Transport(why.to_string()))?;
        let shown = link
            .get_ref()
            .1
            .peer_certificates()
            .and_then(<[_]>::first)
            .cloned();
        let Some((tag, body)) = bounded(frame_async::read_tagged(&mut link)).await?? else {
            return Err(Error::Truncated);
        };
        let said = greeting(tag, &body)?;
        let presented = credential::presented(shown.as_ref(), said.node)?;
        admit(Some(&presented), &said, &self.me)?;

        // Read now, after the peer proved itself, for the reason `greet` gives.
        let holding = Arc::clone(&self.holding);
        let mine = self.store(move || holding.hello()).await?;
        bounded(frame_async::write_tagged(
            &mut link,
            PeerFrame::Hello.tag(),
            &mine.encode(),
        ))
        .await??;

        let asked = match bounded(frame_async::read_tagged(&mut link)).await? {
            Ok(asked) => asked,
            Err(Error::Io(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(why) => return Err(why),
        };
        let voted = match asked {
            None => None,
            // A held stream (ADR-0106 D5): recorded now, because it may stay
            // open for hours and a greeting bound only at its end would leave a
            // joining follower's row unbound all that time.
            Some((tag, body)) if tag == PeerFrame::Stream.tag() => {
                let holding = Arc::clone(&self.holding);
                let met = Met { said, voted: None };
                if let Bridged::Busy(()) | Bridged::Panicked =
                    self.bridge.call((), move |()| holding.met(&met)).await
                {
                    log::warn!("a peer opened a stream but could not be recorded");
                }
                self.stream(&mut link, said.node, body).await?;
                return Ok(None);
            }
            // A copy streams (ADR-0094 D3): the store side runs on the bridge
            // and hands each frame over a bounded channel, so a follower that
            // reads slowly slows the read instead of filling memory, and one
            // that goes away closes the channel and ends the read.
            Some((tag, _)) if tag == PeerFrame::State.tag() => {
                let holding = Arc::clone(&self.holding);
                let node = said.node;
                let (sender, receiver) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(4);
                let copying = self.store(move || {
                    holding.copied(node, &mut |tag, body| {
                        sender.blocking_send((tag, body)).map_err(|_| {
                            Error::Transport("the follower stopped reading the copy".to_owned())
                        })
                    })
                });
                let forwarding = async {
                    let mut receiver = receiver;
                    while let Some((tag, body)) = receiver.recv().await {
                        bounded(frame_async::write_tagged(&mut link, tag, &body)).await??;
                    }
                    Ok::<(), Error>(())
                };
                let (copied, forwarded) = tokio::join!(copying, forwarding);
                forwarded?;
                match copied {
                    Ok(()) => {}
                    Err(Error::Unsubscribed) => {
                        bounded(frame_async::write_tagged(
                            &mut link,
                            PeerFrame::Unsubscribed.tag(),
                            &[],
                        ))
                        .await??;
                    }
                    Err(why) => return Err(why),
                }
                None
            }
            // A request carried here for a caller (ADR-0108 D1–D3). Believed
            // against the certificate THIS handshake proved, so the signer is
            // the peer on this connection and no other.
            Some((tag, body)) if tag == PeerFrame::Coordinate.tag() => {
                let shown = shown.as_ref().ok_or(Error::Unidentified)?;
                let asked = Coordinate::decode(&body)?;
                let believed = asked
                    .signed
                    .verify(
                        shown,
                        said.node,
                        self.me,
                        (asked.digest(), now_ms()),
                        &self.replays,
                    )
                    .copied();
                let (tag, reply) = match believed {
                    Err(why) => {
                        log::warn!(
                            "a request carried from {} was refused: {why}",
                            tessari_types::uuid_to_text(&said.node)
                        );
                        (
                            PeerFrame::NotCoordinated.tag(),
                            why.to_string().into_bytes(),
                        )
                    }
                    Ok(assertion) => {
                        log::info!(
                            "a request carried from {} acts for {} (nonce {})",
                            tessari_types::uuid_to_text(&said.node),
                            match assertion.principal {
                                crate::assertion::Principal::Anonymous => "nobody".to_owned(),
                                crate::assertion::Principal::User { id, .. } =>
                                    format!("user {id}"),
                            },
                            hex(&assertion.nonce)
                        );
                        let holding = Arc::clone(&self.holding);
                        let from = said.node;
                        let answered = self
                            .store(move || Ok(holding.coordinated(from, &assertion, &asked)))
                            .await?;
                        match answered {
                            Ok(answer) => (PeerFrame::Coordinated.tag(), encode_answer(&answer)),
                            Err(why) => (PeerFrame::NotCoordinated.tag(), why.into_bytes()),
                        }
                    }
                };
                bounded(frame_async::write_tagged(&mut link, tag, &reply)).await??;
                None
            }
            Some((tag, body)) => {
                let holding = Arc::clone(&self.holding);
                let voter = Arc::clone(&self.voter);
                let (tag, reply, voted) = self
                    .store(move || answering(tag, &body, &said, &mine, &voter, &*holding))
                    .await?;
                bounded(frame_async::write_tagged(&mut link, tag, &reply)).await??;
                voted
            }
        };
        Ok(Some(Met { said, voted }))
    }

    /// Serve a held stream (ADR-0106 D5) until the follower closes or the door
    /// stops.
    ///
    /// Each ask is answered by exactly ONE round carrying records. While there
    /// is nothing to send the leader reads nothing: it waits on its commit
    /// signal and, every [`STREAM_HEARTBEAT_MILLIS`], sends an empty round — the
    /// heartbeat — which says *nothing after your positions has landed here*.
    /// That claim is exact rather than hopeful because the signal is marked
    /// seen before every read of the log, so a commit landing during a read
    /// wakes the next one instead of being slept past.
    async fn stream(
        &self,
        link: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        follower: [u8; NODE_ID_LEN],
        mut body: Vec<u8>,
    ) -> Result<()> {
        let heartbeat = std::time::Duration::from_millis(STREAM_HEARTBEAT_MILLIS);
        let quiet = crate::collection::Streamed {
            answers: Vec::new(),
        }
        .encode();
        let mut commits = self.holding.commits();
        loop {
            let asked = StreamAsk::decode(&body)?;
            let round = loop {
                commits.borrow_and_update();
                let holding = Arc::clone(&self.holding);
                let ask = asked.clone();
                let answered = self
                    .store(move || stream_answer(&*holding, follower, &ask))
                    .await;
                let round = match answered {
                    Ok(round) => round,
                    // The refusal crosses as the frame a round would carry, and
                    // the stream ends: the follower's round meets it again,
                    // which is where every repair already lives.
                    Err(Error::Uncollectable { from }) => {
                        let mut refused = Vec::with_capacity(8);
                        crate::frame::put_u64(&mut refused, from);
                        bounded(frame_async::write_tagged(
                            link,
                            PeerFrame::Uncollectable.tag(),
                            &refused,
                        ))
                        .await??;
                        return Ok(());
                    }
                    Err(Error::Unsubscribed) => {
                        bounded(frame_async::write_tagged(
                            link,
                            PeerFrame::Unsubscribed.tag(),
                            &[],
                        ))
                        .await??;
                        return Ok(());
                    }
                    Err(why) => return Err(why),
                };
                if !round.is_quiet() {
                    break round;
                }
                // Nothing to send: wait for a commit, saying so every beat.
                loop {
                    tokio::select! {
                        biased;
                        () = self.stop.cancelled() => return Ok(()),
                        changed = commits.changed() => {
                            if changed.is_err() {
                                // The commit signal went with the node.
                                return Ok(());
                            }
                            break;
                        }
                        () = tokio::time::sleep(heartbeat) => {
                            bounded(frame_async::write_tagged(
                                link,
                                PeerFrame::Streamed.tag(),
                                &quiet,
                            ))
                            .await??;
                        }
                    }
                }
            };
            bounded(frame_async::write_tagged(
                link,
                PeerFrame::Streamed.tag(),
                &round.encode(),
            ))
            .await??;
            let next = tokio::select! {
                biased;
                () = self.stop.cancelled() => return Ok(()),
                next = bounded(frame_async::read_tagged(link)) => next?,
            };
            body = match next {
                Ok(Some((tag, next))) if tag == PeerFrame::Stream.tag() => next,
                Ok(Some((tag, _))) => return Err(Error::OutOfTurn { tag }),
                Ok(None) => return Ok(()),
                Err(Error::Io(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(());
                }
                Err(why) => return Err(why),
            };
        }
    }

    /// Run a store call on the bridge, and read a refusal as the error it is.
    async fn store<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        match self.bridge.call((), move |()| work()).await {
            Bridged::Answered(answer) => answer,
            Bridged::Busy(()) => Err(Error::Transport(
                "the peer door is serving as many store calls as it will".to_owned(),
            )),
            Bridged::Panicked => Err(Error::Transport(
                "the store call for this peer panicked".to_owned(),
            )),
        }
    }
}

/// One step of a peer conversation, under the greeting's deadline.
///
/// Per step and not per connection, as the synchronous door's socket timeouts
/// are: a collection answer of several megabytes is many writes, and a bound
/// on the whole would cut off a slow but moving peer.
/// Bytes as lowercase hexadecimal, for a log line.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn bounded<T>(step: impl Future<Output = T>) -> Result<T> {
    tokio::time::timeout(std::time::Duration::from_secs(GREETING_SECONDS), step)
        .await
        .map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "a peer took longer than the greeting deadline",
            ))
        })
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpStream};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::collection::{Collect, Collected, NoLog};
    use crate::gathering::{Gather, Page};
    use crate::grant::{Ballot, Round, Vote};
    use crate::link::tests::{Authority, THERE, hello, settled, voted};
    use crate::link::{Ask, Credential, call};
    use crate::peer::Purpose;
    use tessari_types::{Epoch, Reach};

    const HERE: [u8; NODE_ID_LEN] = [7_u8; NODE_ID_LEN];

    /// A node with no log, holding what [`hello`] says, remembering whom it met.
    struct Holder {
        met: std::sync::Mutex<Vec<Met>>,
        /// Never moved: no test here commits, so a stream would only beat.
        commits: (
            tokio::sync::watch::Sender<u64>,
            tokio::sync::watch::Receiver<u64>,
        ),
    }

    impl Origin for Holder {
        fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
            NoLog.collected(follower, asked)
        }

        fn gathered(&self, asker: [u8; NODE_ID_LEN], asked: &Gather) -> Result<Page> {
            NoLog.gathered(asker, asked)
        }

        fn places(&self, candidate: [u8; NODE_ID_LEN], range: Reach) -> bool {
            NoLog.places(candidate, range)
        }

        fn copied(
            &self,
            follower: [u8; NODE_ID_LEN],
            write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
        ) -> Result<()> {
            NoLog.copied(follower, write)
        }
    }

    impl Holding for Holder {
        fn hello(&self) -> Result<Hello> {
            Ok(hello(HERE))
        }

        fn met(&self, met: &Met) {
            if let Ok(mut held) = self.met.lock() {
                held.push(*met);
            }
        }

        fn commits(&self) -> tokio::sync::watch::Receiver<u64> {
            self.commits.1.clone()
        }

        fn coordinated(
            &self,
            _: [u8; NODE_ID_LEN],
            _: &crate::assertion::Assertion,
            _: &crate::coordination::Coordinate,
        ) -> std::result::Result<tessaridb::Coordinated, String> {
            Err("this test door carries no requests".to_owned())
        }
    }

    /// A door for `HERE`, served on a runtime of its own until the test ends.
    struct Served {
        address: SocketAddr,
        holder: Arc<Holder>,
        stop: CancellationToken,
        serving: tokio::task::JoinHandle<Result<()>>,
        runtime: tokio::runtime::Runtime,
    }

    impl Drop for Served {
        fn drop(&mut self) {
            self.stop.cancel();
        }
    }

    fn served(authority: &Authority) -> Served {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(HERE, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime for the door");
        let holder = Arc::new(Holder {
            met: std::sync::Mutex::new(Vec::new()),
            commits: tokio::sync::watch::channel(0),
        });
        let stop = CancellationToken::new();
        let serving = (stop.clone(), Arc::clone(&holder));
        let serving = runtime.spawn(async move {
            let (stop, holder) = serving;
            peers
                .serve(stop, HERE, Arc::new(Deciding::holding(settled())), holder)
                .await
        });
        Served {
            address,
            holder,
            stop,
            serving,
            runtime,
        }
    }

    fn peer(authority: &Authority) -> Credential {
        authority.issue(THERE, Purpose::Peer)
    }

    #[test]
    fn a_peer_is_greeted_and_its_ballot_answered_by_the_door_on_the_runtime() {
        let authority = Authority::new();
        let door = served(&authority);
        let (said, _) = call(
            door.address,
            peer(&authority),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        )
        .expect("a proved peer is greeted");
        assert_eq!(said.node, HERE);

        let ballot: Ballot = Round::opened(Epoch::new(5), THERE, 3).ballot();
        let (_, answered) = call(
            door.address,
            peer(&authority),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&ballot),
        )
        .expect("a ballot is answered");
        assert_eq!(
            voted(&answered),
            Some(Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            })
        );
        // Recorded through the node, as the synchronous door's caller did. The
        // record is written after the answer, so it is waited for, boundedly.
        let deadline = Instant::now() + Duration::from_secs(GREETING_SECONDS);
        while door.holder.met.lock().map_or(0, |held| held.len()) < 2 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let met = door.holder.met.lock().expect("the record").clone();
        assert_eq!(met.len(), 2, "both connections were recorded");
        assert!(met.iter().any(|m| m.voted
            == Some(Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            })));
    }

    #[test]
    fn a_caller_that_says_nothing_does_not_hold_the_door() {
        let authority = Authority::new();
        let door = served(&authority);
        // A socket that never starts its handshake: a stranger, or a peer that
        // died after connecting. The synchronous door sat on it for the whole
        // greeting deadline before it could take anyone else.
        let _quiet = TcpStream::connect(door.address).expect("a quiet connection");
        let began = Instant::now();
        let (said, _) = call(
            door.address,
            peer(&authority),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        )
        .expect("a proved peer is greeted while a stranger holds a socket");
        let waited = began.elapsed();
        assert_eq!(said.node, HERE);
        assert!(
            waited < Duration::from_secs(GREETING_SECONDS / 2),
            "the second peer waited {waited:?} behind the quiet one"
        );
    }

    #[test]
    fn a_caller_offering_no_credential_is_refused_inside_the_handshake() {
        let authority = Authority::new();
        let door = served(&authority);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(authority.der()).expect("the test authority");
        let settings = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(credential::names(HERE, Purpose::Peer))
            .expect("the door's own name");
        let mut session =
            rustls::ClientConnection::new(Arc::new(settings), name).expect("a client session");
        let mut socket = TcpStream::connect(door.address).expect("a connection");
        socket
            .set_read_timeout(Some(Duration::from_secs(GREETING_SECONDS)))
            .expect("a read deadline");
        let mut link = rustls::Stream::new(&mut session, &mut socket);
        // The greeting a peer would send, if the door let it get that far.
        let sent = std::io::Write::write_all(&mut link, &hello(THERE).encode());
        let mut answer = [0_u8; 1];
        let heard = std::io::Read::read(&mut link, &mut answer);
        assert!(
            sent.is_err() || heard.is_err() || heard.is_ok_and(|read| read == 0),
            "a connection that proved nothing must not be answered"
        );
        // Nobody reached the node: nothing was recorded.
        assert!(door.holder.met.lock().expect("the record").is_empty());
    }

    #[test]
    fn a_stopped_door_returns_and_takes_nobody_new() {
        let authority = Authority::new();
        let mut door = served(&authority);
        door.stop.cancel();
        let serving = &mut door.serving;
        let returned = door.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(GREETING_SECONDS), serving).await
        });
        assert!(
            matches!(returned, Ok(Ok(Ok(())))),
            "a stop ends the door cleanly: {returned:?}"
        );
        let refused = call(
            door.address,
            peer(&authority),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        );
        assert!(refused.is_err(), "a stopped door greets nobody");
    }
}
