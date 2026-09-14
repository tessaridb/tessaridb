//! The link two nodes meet on, and the proof they must show to use it.
//!
//! # Why this is a second door and not the one clients use
//!
//! A client's connection carries a script and a password. A peer's connection
//! carries the log — every namespace, every tenant, every credential hash in the
//! store. The two have nothing in common except a socket, and running them on
//! one door would mean a stranger's bytes reaching the framing that serves the
//! second. So this is its own listener, its own tag space (see
//! [`crate::peer::PeerFrame`]) and its own admission rule.
//!
//! # Mutual, and mutual in both directions for different reasons
//!
//! The listener requires a client certificate, so a connection that proves
//! nothing never reaches a frame at all — it is refused inside the handshake,
//! which is the earliest place a refusal can happen and the cheapest.
//!
//! The dialler's half of *mutual* is quieter and is worth naming, because it
//! looks like it is missing: the caller names the node it means to reach, that
//! name is `<node>.peer.tessari`, and TLS will not complete against a server
//! whose certificate does not carry it. The dialler therefore needs no admission
//! rule of its own — it already refused to talk to the wrong node, before
//! sending a byte of its own greeting.
//!
//! # What is deliberately not here
//!
//! No port is claimed: [`Peers::bind`] takes an address, because which port a
//! cluster peers on is an operator's decision and not this module's. Nothing
//! issues, rotates or revokes a certificate. One connection is served per call
//! to [`Peers::greet`], and ONE follow-up rides it — a ballot or a collection,
//! never both, which is why [`Ask`] is an enum rather than two optional
//! arguments. A node that accepts peers continuously, and holds the connection
//! open between rounds, is a later wave.
//!
//! Nothing decides **when** to stand for leadership or how often to renew. A
//! round is opened by its caller. What this module owes is that the ballot can
//! travel at all and that a refusal arrives as the refusal it was.

use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

use tessari_constants::GREETING_SECONDS;
use tessari_encoding::NODE_ID_LEN;

use crate::collection::{Collect, Collected, Origin};
use crate::credential;
use crate::error::{Error, Result};
use crate::frame;
use crate::grant::{Ballot, Deciding, Vote};
use crate::peer::{Hello, PeerFrame, Purpose, admit};

/// What this node shows a peer, and the key that proves it is ours.
#[derive(Debug)]
pub struct Credential {
    /// The certificate, and any intermediates above it.
    pub chain: Vec<CertificateDer<'static>>,
    /// The private key for the leaf of that chain.
    pub key: PrivateKeyDer<'static>,
}

/// A door peers arrive at.
#[derive(Debug)]
pub struct Peers {
    listener: TcpListener,
    settings: Arc<ServerConfig>,
}

impl Peers {
    /// Open the peer door at `address`, trusting exactly `authority`.
    ///
    /// One root and not a platform store: a cluster's peers are issued by the
    /// cluster, and a link that would also accept a certificate from any public
    /// authority is a link whose membership is whatever the operating system was
    /// shipped believing.
    ///
    /// # Errors
    ///
    /// Returns the socket's own failure, or [`Error::Transport`] when the
    /// credential or the authority will not make a usable configuration.
    pub fn bind(
        address: impl ToSocketAddrs,
        mine: Credential,
        authority: &CertificateDer<'_>,
    ) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        roots
            .add(authority.clone().into_owned())
            .map_err(|why| Error::Transport(why.to_string()))?;
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|why| Error::Transport(why.to_string()))?;
        let settings = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(mine.chain, mine.key)
            .map_err(|why| Error::Transport(why.to_string()))?;
        Ok(Self {
            listener: TcpListener::bind(address)?,
            settings: Arc::new(settings),
        })
    }

    /// Where the door actually is, which matters when the port was asked for as zero.
    ///
    /// # Errors
    ///
    /// Returns the socket's own failure.
    pub fn address(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Take one peer, prove who it is, and answer with what `mine` says now.
    ///
    /// The order is deliberate and is the module's whole argument: the
    /// credential is settled first, the greeting is read second, and this node
    /// says what it holds only after both. A node that greeted first would be
    /// telling an unproven stranger its epoch and how far its log reaches.
    ///
    /// # Why `mine` is a closure and not a value
    ///
    /// This function accepts **inside itself**, so everything a caller computes
    /// before the call is computed before the wait. A `Hello` is entirely a
    /// claim about *state* — epoch, roles, log tail, how old this copy is — and
    /// a door that sat idle for an hour would have greeted with hour-old facts.
    /// The tail and the copy's age are exactly what a router reads, and a
    /// staleness bound applied to an hour-old answer excludes or admits a node
    /// that no longer exists.
    ///
    /// It is called at the latest moment that is still honest: after the
    /// credential is settled and the peer's own greeting is heard, immediately
    /// before this node answers. Later is not possible, and anywhere earlier
    /// re-opens the gap by however long the step it precedes takes.
    ///
    /// # Why `me` is a value while `mine` is a closure
    ///
    /// They look like the same fact read twice and are not. `mine` is a claim
    /// about **state** — epoch, roles, log tail, how old this copy is — and is
    /// therefore read on arrival. `me` is this node's identity, which is fixed
    /// when the store is initialised and cannot go stale however long the door
    /// waits, so it is settled before the wait and used by the refusal that runs
    /// before this node says anything at all.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unidentified`] when nothing was presented,
    /// [`Error::CredentialNamesAnother`] when what was presented does not name
    /// the node the greeting claims, [`Error::NotAPeerCredential`] when it
    /// names that node for the client link instead of this one,
    /// [`Error::ClaimsOurOwnIdentity`] when the peer arrives under this node's
    /// own id, and whatever `mine` returns when this node cannot state what it
    /// holds.
    pub fn greet(
        &self,
        mine: impl FnOnce() -> Result<Hello>,
        me: &[u8; NODE_ID_LEN],
        voter: &Deciding,
        log: &dyn Origin,
    ) -> Result<Met> {
        let (mut socket, _) = self.listener.accept()?;
        let bound = Some(Duration::from_secs(GREETING_SECONDS));
        socket.set_read_timeout(bound)?;
        socket.set_write_timeout(bound)?;
        let mut session = ServerConnection::new(Arc::clone(&self.settings))
            .map_err(|why| Error::Transport(why.to_string()))?;
        session
            .complete_io(&mut socket)
            .map_err(|why| Error::Transport(why.to_string()))?;

        let shown = session.peer_certificates().and_then(<[_]>::first).cloned();
        let mut link = rustls::Stream::new(&mut session, &mut socket);
        let said = hear(&mut link)?;
        let presented = credential::presented(shown.as_ref(), said.node)?;
        admit(Some(&presented), &said, me)?;
        // Read here and not before the accept above: see the note on this
        // function. A peer has arrived and proved who it is, so the facts this
        // node is about to state are the ones it holds at the moment it states
        // them.
        let mine = mine()?;
        say(&mut link, &mine)?;

        // Whatever the peer asks next rides the connection the greeting opened.
        // Nothing at all is a peer that only wanted to know who we are — and a
        // peer that vanished after its greeting reads the same way here, on
        // purpose: the exchange this connection was opened for completed, and
        // there is no half-read frame to be wrong about. A truncated frame is
        // still `Truncated`, because that failure happens after a header.
        let asked = match frame::read_tagged(&mut link) {
            Ok(asked) => asked,
            Err(Error::Io(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(why) => return Err(why),
        };
        let voted = match asked {
            None => None,
            Some((tag, body)) => match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Collect) => {
                    let asked = Collect::decode(&body)?;
                    // The follower names itself by the id the handshake proved,
                    // never by one it writes into a frame — so a peer cannot
                    // record somebody else's progress, and the per-follower lag
                    // report stays per follower.
                    match log.collected(said.node, asked) {
                        Ok(collected) => frame::write_tagged(
                            &mut link,
                            PeerFrame::Collected.tag(),
                            &collected.encode(),
                        )?,
                        // A refusal crosses the wire as the refusal it was. The
                        // alternative is closing the socket, which reaches the
                        // other end as a truncated conversation and sends
                        // whoever reads it to look for a network fault.
                        Err(Error::Uncollectable { from }) => {
                            let mut refused = Vec::with_capacity(8);
                            frame::put_u64(&mut refused, from);
                            frame::write_tagged(
                                &mut link,
                                PeerFrame::Uncollectable.tag(),
                                &refused,
                            )?;
                        }
                        // The same rule one step out: a node nobody
                        // subscribed learns that, rather than watching its
                        // socket close and reading it as a network fault.
                        Err(Error::Unsubscribed) => {
                            frame::write_tagged(&mut link, PeerFrame::Unsubscribed.tag(), &[])?
                        }
                        Err(why) => return Err(why),
                    }
                    None
                }
                Some(PeerFrame::Ballot) => {
                    let asked = Ballot::decode(&body)?;
                    // The identity that decides a grant is the one the
                    // handshake proved, never the one the frame claims. Checked
                    // here rather than inside the voter because this is the only
                    // place both are in scope, and because a rule that took a
                    // proved identity as an argument would be a rule that could
                    // be handed an unproved one.
                    if asked.candidate != said.node {
                        return Err(Error::NotItsOwnBallot);
                    }
                    // The candidate's log position comes from the greeting it
                    // proved a moment ago on this connection, for the reason the
                    // line above gives about its identity: a position a
                    // candidate writes into the ballot being judged is a
                    // position it can choose.
                    let vote = voter.asked(
                        &asked,
                        std::time::Instant::now(),
                        mine.reached(),
                        said.reached(),
                    );
                    frame::write_tagged(&mut link, PeerFrame::Vote.tag(), &vote.encode())?;
                    Some(vote)
                }
                Some(_) => return Err(Error::OutOfTurn { tag }),
                None => return Err(Error::UnknownFrame { tag }),
            },
        };
        Ok(Met { said, voted })
    }
}

impl Credential {
    /// A second copy of this credential, for the next door.
    ///
    /// [`call`] takes ownership because a TLS client configuration does, and a
    /// node that speaks to more than one peer therefore needs one copy per
    /// conversation.
    ///
    /// This was `pub(crate)` while every caller that spoke to N peers lived in
    /// this crate, on the argument that duplicating key material is a detail
    /// rather than a capability worth publishing. W239 moved one of those
    /// callers into the binary: the dialling thread holds the node's credential
    /// and opens a session per peer per round, while [`Peers::bind`] has already
    /// consumed the copy that answers the door. The argument for hiding it was
    /// about where the callers were, and they are no longer all here.
    ///
    /// It stays a named method rather than a `Clone` impl for the original
    /// reason: a private key that copies itself wherever `.clone()` is
    /// convenient is a key whose copies nobody is counting.
    #[must_use]
    pub fn duplicate(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            key: self.key.clone_key(),
        }
    }
}

/// What one served connection produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Met {
    /// What the peer said it holds.
    pub said: Hello,
    /// How this node voted, when the peer asked for an epoch.
    ///
    /// `None` covers three different connections — a peer that asked nothing, a
    /// peer that collected records, and one that vanished after its greeting —
    /// because none of them produced a vote and this field is about votes. What
    /// a collection produced is recorded against the follower in the store
    /// rather than handed back here, since the report that matters is the
    /// leader's per-follower lag and not one connection's outcome.
    pub voted: Option<Vote>,
}

/// What a caller asks for on the connection its greeting opened.
///
/// An enum and not two optional arguments, because one connection carries one
/// follow-up: a caller able to pass both would be expressing a conversation the
/// door cannot serve and nothing would refuse it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Ask<'a> {
    /// Nothing — the caller only wanted to know who is there.
    Nothing,
    /// One epoch.
    Ballot(&'a Ballot),
    /// The records after a position this caller does not hold.
    Records(Collect),
}

/// What the other end answered with.
///
/// Paired with [`Ask`] on purpose: an answer of the wrong kind is a peer
/// speaking this protocol incorrectly and is refused as
/// [`Error::OutOfTurn`], rather than accepted because it happened to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answered {
    /// Nothing was asked, so nothing was answered.
    Nothing,
    /// How the peer voted.
    Voted(Vote),
    /// What the peer's log held.
    Collected(Collected),
}

/// Reach the peer `at` on `address`, and exchange greetings.
///
/// # Errors
///
/// Returns [`Error::Transport`] when the node reached does not hold a peer
/// credential for `at` — the handshake refuses it, which is why this side needs
/// no admission rule of its own; [`Error::Uncollectable`] when a collection was
/// asked for from a position the peer cannot state a predecessor for; and
/// [`Error::OutOfTurn`] when the answer is not of the kind that was asked for.
pub fn call(
    address: impl ToSocketAddrs,
    mine: Credential,
    authority: &CertificateDer<'_>,
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Ask<'_>,
) -> Result<(Hello, Answered)> {
    let mut roots = RootCertStore::empty();
    roots
        .add(authority.clone().into_owned())
        .map_err(|why| Error::Transport(why.to_string()))?;
    let settings = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(mine.chain, mine.key)
        .map_err(|why| Error::Transport(why.to_string()))?;
    let expected = credential::names(at, Purpose::Peer);
    let name = ServerName::try_from(expected).map_err(|why| Error::Transport(why.to_string()))?;
    let mut session = ClientConnection::new(Arc::new(settings), name)
        .map_err(|why| Error::Transport(why.to_string()))?;

    let mut socket = TcpStream::connect(address)?;
    let bound = Some(Duration::from_secs(GREETING_SECONDS));
    socket.set_read_timeout(bound)?;
    socket.set_write_timeout(bound)?;
    let exchanged = exchange(&mut session, &mut socket, said, asking);

    // Say goodbye properly even when the exchange failed. A TLS peer that just
    // drops the socket makes the other end's next read an error rather than an
    // end, so a caller that skipped this would leave every door it spoke to
    // reporting a fault it did not have.
    session.send_close_notify();
    drop(session.write_tls(&mut socket));
    exchanged
}

/// The greeting and the one follow-up, on a session that is already open.
fn exchange(
    session: &mut ClientConnection,
    socket: &mut TcpStream,
    said: &Hello,
    asking: Ask<'_>,
) -> Result<(Hello, Answered)> {
    let mut link = rustls::Stream::new(session, socket);
    say(&mut link, said)?;
    let heard = hear(&mut link)?;

    match asking {
        Ask::Nothing => Ok((heard, Answered::Nothing)),
        Ask::Ballot(ballot) => {
            frame::write_tagged(&mut link, PeerFrame::Ballot.tag(), &ballot.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Vote) => Ok((heard, Answered::Voted(Vote::decode(&body)?))),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Records(collect) => {
            frame::write_tagged(&mut link, PeerFrame::Collect.tag(), &collect.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Collected) => {
                    Ok((heard, Answered::Collected(Collected::decode(&body)?)))
                }
                // The refusal the leader sent, rebuilt as the value it was on
                // the other side. A follower that received this as a closed
                // socket would be looking for a network fault instead of
                // reading the one sentence that says what to do.
                Some(PeerFrame::Uncollectable) => {
                    let (from, _) = frame::take_u64(&body, 0)?;
                    Err(Error::Uncollectable { from })
                }
                Some(PeerFrame::Unsubscribed) => Err(Error::Unsubscribed),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
    }
}

/// The one frame that answers the one follow-up.
fn answer(link: &mut rustls::Stream<'_, ClientConnection, TcpStream>) -> Result<(u8, Vec<u8>)> {
    frame::read_tagged(link)?.ok_or(Error::Truncated)
}

/// Put one greeting on the link.
fn say(link: &mut impl std::io::Write, hello: &Hello) -> Result<()> {
    frame::write_tagged(link, PeerFrame::Hello.tag(), &hello.encode())
}

/// Take one greeting off the link, and refuse anything else.
fn hear(link: &mut impl std::io::Read) -> Result<Hello> {
    let Some((tag, body)) = frame::read_tagged(link)? else {
        return Err(Error::Truncated);
    };
    match PeerFrame::from_tag(tag) {
        Some(PeerFrame::Hello) => Hello::decode(&body),
        Some(_) => Err(Error::OutOfTurn { tag }),
        None => Err(Error::UnknownFrame { tag }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Answered, Ask, Credential, Met, Peers, Result, call};
    use crate::collection::{Collect, NoLog};
    use crate::credential::names;
    use crate::error::Error;
    use crate::grant::{Ballot, Deciding, Refused, Round, Vote, Voter};
    use crate::peer::{Hello, Purpose};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use tessari_encoding::{NODE_ID_LEN, NodeIdentity};
    use tessari_types::{Epoch, Sequence};

    /// A certificate authority that exists for the length of one test.
    ///
    /// Minted in memory on purpose: a fixture on disk is key material in a
    /// repository, and a fixture with an expiry date is a test that fails on a
    /// day nobody chose.
    pub(crate) struct Authority {
        certificate: rcgen::Certificate,
        key: rcgen::KeyPair,
    }

    impl Authority {
        pub(crate) fn new() -> Self {
            let mut params =
                rcgen::CertificateParams::new(Vec::new()).expect("an authority's parameters");
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let key = rcgen::KeyPair::generate().expect("an authority's key");
            let certificate = params.self_signed(&key).expect("a self-signed authority");
            Self { certificate, key }
        }

        pub(crate) fn der(&self) -> CertificateDer<'static> {
            CertificateDer::from(self.certificate.der().to_vec())
        }

        /// Issue a credential naming `node` for `purpose`.
        pub(crate) fn issue(&self, node: [u8; NODE_ID_LEN], purpose: Purpose) -> Credential {
            self.named(&names(node, purpose))
        }

        fn named(&self, name: &str) -> Credential {
            let params =
                rcgen::CertificateParams::new(vec![name.to_owned()]).expect("a leaf's parameters");
            let key = rcgen::KeyPair::generate().expect("a leaf's key");
            let leaf = params
                .signed_by(&key, &self.certificate, &self.key)
                .expect("a leaf signed by the authority");
            Credential {
                chain: vec![CertificateDer::from(leaf.der().to_vec())],
                key: PrivateKeyDer::try_from(key.serialize_der()).expect("a usable leaf key"),
            }
        }
    }

    /// The vote inside an answer, or `None` when the peer answered otherwise.
    pub(crate) fn voted(answered: &Answered) -> Option<Vote> {
        match answered {
            Answered::Voted(vote) => Some(*vote),
            _ => None,
        }
    }

    fn identity(node: [u8; NODE_ID_LEN]) -> NodeIdentity {
        NodeIdentity::alone(node)
    }

    pub(crate) fn hello(node: [u8; NODE_ID_LEN]) -> Hello {
        Hello::about(
            &identity(node),
            Epoch::new(4),
            Sequence::new(9),
            LEVEL.leadership,
            Some(core::time::Duration::ZERO),
        )
    }

    /// The log position every greeting here carries, so that two nodes built by
    /// [`hello`] are level and a case about the handshake is not also a case
    /// about the election restriction.
    pub(crate) const LEVEL: crate::grant::Reached = crate::grant::Reached {
        leadership: Epoch::new(3),
        tail: Sequence::new(9),
    };

    /// A voter that has been up long enough to have outlived anything it could
    /// have granted before a restart — otherwise every door in these tests
    /// would refuse on the rule that has nothing to do with what is being
    /// tested.
    pub(crate) fn settled() -> Voter {
        Voter::started_at(
            std::time::Instant::now()
                .checked_sub(tessari_storage::LEASE_TTL)
                .expect("this machine has been up for ten seconds"),
        )
    }

    const HERE: [u8; NODE_ID_LEN] = [1_u8; NODE_ID_LEN];
    pub(crate) const THERE: [u8; NODE_ID_LEN] = [2_u8; NODE_ID_LEN];

    /// Open a door for `HERE` and hand back where it is, plus the outcome.
    fn door(authority: &Authority) -> (Peers, Hello) {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(HERE, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        (peers, hello(HERE))
    }

    #[test]
    fn two_nodes_that_prove_who_they_are_exchange_what_they_hold() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
        });

        let theirs = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        )
        .expect("a peer that proved itself is answered")
        .0;

        let heard = listening
            .join()
            .expect("the door's thread")
            .expect("the door admits a peer credential naming the greeter");
        // Each end learned the other's facts, and neither learned them from a
        // certificate: the epoch and the tail are in the frame because a
        // credential outlives both.
        assert_eq!(heard.said.node, THERE);
        assert_eq!(heard.said.epoch, Epoch::new(4));
        assert_eq!(heard.said.tail, Sequence::new(9));
        assert_eq!(heard.voted, None, "nobody asked for anything");
        assert_eq!(theirs.node, HERE);
    }

    #[test]
    fn the_greeting_carries_what_this_node_holds_when_the_peer_arrives_not_when_the_door_opened() {
        let authority = Authority::new();
        let (peers, _) = door(&authority);
        let address = peers.address().expect("the door's address");

        // The node's log tail, which moves while the door is waiting. A `Hello`
        // is entirely a claim about state, and this is the field a router reads
        // beside the copy's age — so *when* it was read is the whole criterion.
        let tail = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(9));
        let read = std::sync::Arc::clone(&tail);
        let (entered, waiting) = std::sync::mpsc::channel();

        let listening = std::thread::spawn(move || {
            // Sent before `greet`, so the advance below cannot land while this
            // thread is still being scheduled.
            entered.send(()).expect("the test is still listening");
            peers.greet(
                || {
                    Ok(Hello::about(
                        &identity(HERE),
                        Epoch::new(4),
                        Sequence::new(read.load(std::sync::atomic::Ordering::SeqCst)),
                        LEVEL.leadership,
                        Some(core::time::Duration::ZERO),
                    ))
                },
                &HERE,
                &Deciding::holding(settled()),
                &NoLog,
            )
        });
        waiting.recv().expect("the door's thread starts");
        // The pause is for the FALSIFICATION and not for this assertion. Reading
        // on arrival is correct whatever the timing, because the closure cannot
        // run until `accept` returns and `accept` cannot return until the
        // connection below is made. Restore the eager read and the door has
        // microseconds in which to take the stale value — this widens that
        // window so the arm bites every run instead of most of them.
        std::thread::sleep(core::time::Duration::from_millis(100));
        tail.store(41, std::sync::atomic::Ordering::SeqCst);

        let theirs = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        )
        .expect("a peer that proved itself is answered")
        .0;

        listening
            .join()
            .expect("the door's thread")
            .expect("the door admits a peer credential naming the greeter");
        // 41 and not 9: the door greeted with what this node held when the peer
        // arrived, not with what it held an idle stretch earlier.
        assert_eq!(
            theirs.tail,
            Sequence::new(41),
            "the greeting carries the tail read on arrival, not the one read when the door opened"
        );
    }

    #[test]
    fn a_door_with_no_log_refuses_a_collection_as_a_refusal_and_not_by_hanging_up() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
        });

        let failure = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Records(Collect {
                from: Sequence::new(7),
                limit: 16,
            }),
        )
        .expect_err("a door with no log behind it has nothing to hand over");

        // The position it asked from, handed back. That is what tells the
        // follower *not from here* apart from *you are level*, and it is the
        // whole reason this is `Uncollectable` and not a dropped socket: a
        // conversation that ends mid-frame reaches whoever reads it as a network
        // fault and sends them to a packet capture.
        assert!(
            matches!(failure, Error::Uncollectable { from: 7 }),
            "{failure}"
        );
        // And the door itself finished the conversation rather than failing:
        // it served the greeting, refused the ask, and closed in order.
        let heard = listening
            .join()
            .expect("the door's thread")
            .expect("a refused collection is a served connection, not a failed one");
        assert_eq!(heard.said.node, THERE);
        assert_eq!(heard.voted, None, "a collection is not a vote");
    }

    #[test]
    fn a_client_credential_on_the_peer_link_is_refused_on_its_purpose() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
        });

        // The id is perfectly correct. What is wrong is the link it was issued
        // for, which is the criterion's own sentence.
        drop(call(
            address,
            authority.issue(THERE, Purpose::Client),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        ));

        let refused = listening
            .join()
            .expect("the door's thread")
            .expect_err("a client credential is not a peer credential");
        assert!(matches!(refused, Error::NotAPeerCredential), "{refused}");
    }

    #[test]
    fn a_credential_that_does_not_name_the_greeter_is_refused_and_names_the_file() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
        });

        // Issued by the right authority, for the right link, for the wrong node.
        drop(call(
            address,
            authority.issue([3_u8; NODE_ID_LEN], Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Nothing,
        ));

        let refused = listening
            .join()
            .expect("the door's thread")
            .expect_err("a credential that names another node is refused");
        let said = refused.to_string();
        // The refusal is read by a person, so it is the rendering that is
        // asserted: it must carry a fingerprint, because that is the only thing
        // here that identifies one file on one machine.
        assert!(
            matches!(refused, Error::CredentialNamesAnother { .. }),
            "{said}"
        );
        assert!(said.contains("sha256 "), "{said}");
    }

    /// A door that answers one ballot, on its own thread, with its own voter.
    ///
    /// Each door is a separate voting member with a separate memory, which is
    /// the only shape in which a majority means anything.
    pub(crate) fn voting(
        authority: &Authority,
        id: [u8; NODE_ID_LEN],
        voter: Voter,
    ) -> (SocketAddr, JoinHandle<Result<Met>>) {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(id, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let mine = hello(id);
        let deciding = Deciding::holding(voter);
        let answering =
            std::thread::spawn(move || peers.greet(|| Ok(mine), &HERE, &deciding, &NoLog));
        (address, answering)
    }

    /// A settled voter that has already granted the epoch before the one under
    /// test — the ordinary state of a voting member in a cluster that has a
    /// leader.
    fn incumbent() -> Voter {
        let mut voter = settled();
        let _granted = voter.asked(
            &Ballot {
                epoch: Epoch::new(1),
                candidate: HERE,
            },
            std::time::Instant::now(),
            LEVEL,
            LEVEL,
        );
        voter
    }

    #[test]
    fn a_ballot_crosses_the_link_and_comes_back_a_vote() {
        let authority = Authority::new();
        let (address, answering) = voting(&authority, HERE, settled());

        let ballot = Ballot {
            epoch: Epoch::new(12),
            candidate: THERE,
        };
        let (_, vote) = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&ballot),
        )
        .expect("a peer that proved itself may ask");

        assert_eq!(voted(&vote), Some(Vote::Granted));
        let met = answering
            .join()
            .expect("the door's thread")
            .expect("served");
        assert_eq!(met.voted, Some(Vote::Granted), "both ends saw one answer");
    }

    /// A greeting from a node whose log stops short of [`LEVEL`].
    fn falling_behind(node: [u8; NODE_ID_LEN]) -> Hello {
        Hello::about(
            &identity(node),
            Epoch::new(4),
            Sequence::new(LEVEL.tail.get().saturating_sub(3)),
            LEVEL.leadership,
            Some(core::time::Duration::ZERO),
        )
    }

    #[test]
    fn a_candidate_whose_log_is_behind_is_refused_at_the_door() {
        // The wiring test for ADR-0063's second half, and it is the half a unit
        // test cannot reach: the rule lives in the voter, but the position it
        // judges has to arrive from the GREETING the candidate proved rather
        // than from the ballot it wrote. A door that passed the ballot's word
        // for it would pass every unit test in `grant` and restrict nothing.
        let authority = Authority::new();
        let (address, answering) = voting(&authority, HERE, settled());

        let (_, vote) = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &falling_behind(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(12),
                candidate: THERE,
            }),
        )
        .expect("a peer that proved itself may ask");

        assert_eq!(
            voted(&vote),
            Some(Vote::Refused(Refused::LogBehind {
                leadership: LEVEL.leadership,
                tail: LEVEL.tail,
            })),
            "the door judged the position the candidate greeted with"
        );
        let met = answering
            .join()
            .expect("the door's thread")
            .expect("served");
        assert_eq!(met.voted, voted(&vote), "both ends saw one answer");
    }

    #[test]
    fn a_ballot_naming_somebody_else_never_reaches_the_voter() {
        // The hole W228 opens and closes in the same wave. A voter now grants a
        // ballot from the node it is already holding a grant for — so a peer
        // free to write the incumbent's id into its own ballot would collect
        // exactly the grants the liveness rule exists to withhold, and the
        // cluster would have two holders.
        //
        // The credential says THERE and the ballot says HERE. Refused at the
        // door, before the voter is asked anything at all.
        let authority = Authority::new();
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(HERE, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");

        let mine = hello(HERE);
        let answering = std::thread::spawn(move || {
            let voter = Deciding::holding(settled());
            let met = peers.greet(|| Ok(mine), &HERE, &voter, &NoLog);
            // The voter is handed back untouched: nothing was decided, which is
            // the half a refusal-shaped answer would not have given.
            (met, voter.decided())
        });

        let _ = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(1),
                candidate: HERE,
            }),
        );

        let (met, decided) = answering.join().expect("the door's thread");
        assert!(
            matches!(met, Err(Error::NotItsOwnBallot)),
            "expected the door to refuse the ballot outright, got {met:?}"
        );
        assert_eq!(decided, None, "the voter was never asked");
    }

    #[test]
    fn a_refusal_keeps_its_reason_and_its_wait_across_the_wire() {
        let authority = Authority::new();
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(HERE, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");

        // A grant this voter is already holding for somebody ELSE, so what
        // crosses the wire is a challenger and not a renewal. W228 made that
        // distinction decide the vote: the same candidate asking again is
        // granted, because re-granting to the holder adds no second holder.
        let mine = hello(HERE);
        let answering = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &Deciding::holding(incumbent()), &NoLog)
        });

        let refused = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(2),
                candidate: THERE,
            }),
        )
        .expect("a peer that proved itself may ask")
        .1;
        let refused = voted(&refused).expect("a vote came back");
        drop(answering.join().expect("the door's thread"));

        // The reason survives, and so does the wait: a candidate told only "no"
        // cannot tell waiting from being wrong.
        match refused {
            Vote::Refused(Refused::EarlierGrantStillAlive { for_the_next }) => {
                assert!(
                    for_the_next > Duration::ZERO && for_the_next <= tessari_storage::LEASE_TTL,
                    "{for_the_next:?}"
                );
            }
            other => unreachable!("expected a live-grant refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_candidate_that_reaches_a_majority_holds_the_epoch() {
        let authority = Authority::new();
        let doors: Vec<_> = [
            [10_u8; NODE_ID_LEN],
            [11_u8; NODE_ID_LEN],
            [12_u8; NODE_ID_LEN],
        ]
        .into_iter()
        .map(|id| (id, voting(&authority, id, settled())))
        .collect();

        let mut round = Round::opened(Epoch::new(5), THERE, doors.len());
        let mut held = None;
        for (id, (address, _)) in &doors {
            let (_, vote) = call(
                *address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                *id,
                &hello(THERE),
                Ask::Ballot(&round.ballot()),
            )
            .expect("every door is up");
            held = round.counts(*id, voted(&vote).expect("a door that was asked answers"));
        }

        for (_, (_, answering)) in doors {
            drop(answering.join().expect("the door's thread"));
        }
        let held = held.expect("three of three carried it");
        assert_eq!(held.epoch, Epoch::new(5));
    }

    #[test]
    fn a_challenger_a_majority_refuses_holds_nothing() {
        let authority = Authority::new();
        // Every door is up and every door says no, because a leader already
        // holds the epoch before this one. This is the ordinary failure — far
        // more common than a partition — and it is the one where a candidate
        // that counted answers rather than grants would elect itself.
        let doors: Vec<_> = [
            [40_u8; NODE_ID_LEN],
            [41_u8; NODE_ID_LEN],
            [42_u8; NODE_ID_LEN],
        ]
        .into_iter()
        .map(|id| (id, voting(&authority, id, incumbent())))
        .collect();

        let mut round = Round::opened(Epoch::new(2), THERE, doors.len());
        for (id, (address, _)) in &doors {
            let (_, vote) = call(
                *address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                *id,
                &hello(THERE),
                Ask::Ballot(&round.ballot()),
            )
            .expect("every door is up and answering");
            let vote = voted(&vote).expect("a door that was asked answers");
            assert!(
                matches!(vote, Vote::Refused(Refused::EarlierGrantStillAlive { .. })),
                "{vote:?}"
            );
            assert_eq!(round.counts(*id, vote), None, "a refusal is not a grant");
        }

        for (_, (_, answering)) in doors {
            drop(answering.join().expect("the door's thread"));
        }
        assert_eq!(round.held(), None, "three noes are not a majority of yeses");
    }

    #[test]
    fn a_candidate_partitioned_from_the_majority_holds_nothing() {
        let authority = Authority::new();
        // Three voting members configured; one door is up. The other two are
        // not refusing — they are gone, which is what a partition looks like
        // from here and is the only version of it worth testing.
        let alive = [20_u8; NODE_ID_LEN];
        let (address, answering) = voting(&authority, alive, settled());
        let unreachable = Peers::bind(
            "127.0.0.1:0",
            authority.issue([21_u8; NODE_ID_LEN], Purpose::Peer),
            &authority.der(),
        )
        .expect("a door, briefly");
        let vanished = unreachable.address().expect("its address");
        drop(unreachable);

        let mut round = Round::opened(Epoch::new(9), THERE, 3);
        let (_, vote) = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            alive,
            &hello(THERE),
            Ask::Ballot(&round.ballot()),
        )
        .expect("the one door that is up answers");
        assert_eq!(
            round.counts(alive, voted(&vote).expect("it answered")),
            None,
            "one of three is not a majority"
        );

        let reached = call(
            vanished,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            [21_u8; NODE_ID_LEN],
            &hello(THERE),
            Ask::Ballot(&round.ballot()),
        );
        assert!(reached.is_err(), "a door that is gone answers nothing");

        drop(answering.join().expect("the door's thread"));
        assert_eq!(round.held(), None, "the round never concluded");
    }

    #[test]
    fn a_leader_that_could_not_renew_refuses_writes_before_its_lease_expires() {
        let authority = Authority::new();
        let store = tessaridb::Db::in_memory().expect("a store");
        store
            .session()
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE COLLECTION users;",
            )
            .expect("a place to write");

        // A majority grants, and the node takes the lease that grant entitles it
        // to. The span is short so the fence is reachable inside a test; the
        // arithmetic it runs is the same one a ten-second lease runs.
        let voters = [
            [30_u8; NODE_ID_LEN],
            [31_u8; NODE_ID_LEN],
            [32_u8; NODE_ID_LEN],
        ];
        let doors: Vec<_> = voters
            .into_iter()
            .map(|id| (id, voting(&authority, id, settled())))
            .collect();

        let mut round = Round::opened(Epoch::new(1), THERE, voters.len());
        let mut held = None;
        for (id, (address, _)) in &doors {
            let (_, vote) = call(
                *address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                *id,
                &hello(THERE),
                Ask::Ballot(&round.ballot()),
            )
            .expect("every door is up");
            held = round.counts(*id, voted(&vote).expect("it answered"));
        }
        let held = held.expect("three of three carried it");

        // The majority goes away — every door joined and dropped, so the
        // addresses are real and nothing is listening on them. That is the
        // partition, and it is a partition of the whole majority rather than of
        // one convenient peer.
        let addresses: Vec<_> = doors
            .into_iter()
            .map(|(id, (address, answering))| {
                drop(answering.join().expect("the door's thread"));
                (id, address)
            })
            .collect();

        let ttl = tessari_storage::LEASE_GUARD
            .checked_add(Duration::from_millis(400))
            .expect("representable");
        let taken = std::time::Instant::now();
        store.hold_lease(ttl);
        assert_eq!(held.epoch, Epoch::new(1));
        store
            .session()
            .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:1 = { name: 'ada' };")
            .expect("a leader inside its window writes");

        // Now the partition: the voter is gone, so the renewal round cannot
        // reach anyone, let alone a majority, and nothing renews.
        let renewal = Round::opened(Epoch::new(2), THERE, addresses.len());
        for (id, address) in &addresses {
            let reached = call(
                *address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                *id,
                &hello(THERE),
                Ask::Ballot(&renewal.ballot()),
            );
            assert!(reached.is_err(), "the majority is unreachable");
        }
        assert_eq!(renewal.held(), None, "so the renewal grants nothing");

        // Past the fence, which is `ttl - GUARD` = 400 ms, and comfortably
        // short of the expiry at 2.4 s. That gap is the whole point: the holder
        // stops writing while the cluster still may not reassign.
        std::thread::sleep(Duration::from_millis(600));
        let refused = store
            .session()
            .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:2 = { name: 'grace' };")
            .expect_err("a leader that could not renew stops writing");
        let elapsed = taken.elapsed();

        // The criterion's own sentence, measured on monotonic elapsed time: the
        // refusal happened, and it happened strictly before the lease expired.
        let fence = ttl
            .checked_sub(tessari_storage::LEASE_GUARD)
            .expect("a ttl longer than the guard");
        assert!(elapsed >= fence, "refused before the fence: {elapsed:?}");
        assert!(
            elapsed < ttl,
            "refused after the expiry, not before it: {elapsed:?}"
        );
        let said = refused.to_string();
        assert!(said.contains("lease"), "{said}");
    }

    #[test]
    fn the_lease_a_round_won_is_the_lease_the_node_holds() {
        // The seam between a round and the fence, asserted without a socket
        // because the socket is not what is in question. A granted lease is
        // dated from the instant its round OPENED, and installing it has to
        // carry that instant: a span cannot, because by the time one arrives the
        // collection delay has already been spent, and restarting the clock here
        // would spend it a second time out of the VOTERS' window instead of this
        // node's — which is the split-brain the dating rule exists to prevent,
        // reached through the seam rather than through the rule.
        let store = tessaridb::Db::in_memory().expect("a store");
        store
            .session()
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE COLLECTION users;",
            )
            .expect("a place to write");
        let voter = [40_u8; NODE_ID_LEN];

        // A round that opened now and was carried at once.
        let mut prompt = Round::opened(Epoch::new(1), THERE, 1);
        let won = prompt
            .counts(voter, Vote::Granted)
            .expect("one of one carries it");
        store.hold(won.epoch, won.lease());
        store
            .session()
            .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:1 = { name: 'ada' };")
            .expect("a round that cost nothing hands over the whole window");

        // The same round, opened a whole TTL ago. Nothing else differs.
        let opened = std::time::Instant::now()
            .checked_sub(tessari_storage::LEASE_TTL)
            .expect("representable");
        let mut slow = Round::opened_at(Epoch::new(2), THERE, 1, opened);
        let won = slow
            .counts(voter, Vote::Granted)
            .expect("one of one carries it");
        store.hold(won.epoch, won.lease());
        let refused = store
            .session()
            .run("USE NAMESPACE prod; USE DATABASE orders; CREATE users:2 = { name: 'grace' };")
            .expect_err("a round that took the whole TTL hands over no window at all");
        let said = refused.to_string();
        assert!(said.contains("lease"), "{said}");
    }

    #[test]
    fn a_connection_offering_no_credential_never_reaches_a_frame() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &Deciding::holding(settled()), &NoLog)
        });

        let mut roots = rustls::RootCertStore::empty();
        roots.add(authority.der()).expect("the test authority");
        let settings = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(names(HERE, Purpose::Peer))
            .expect("the door's own name");
        let mut session =
            rustls::ClientConnection::new(Arc::new(settings), name).expect("a client session");
        if let Ok(mut socket) = TcpStream::connect(address) {
            let mut link = rustls::Stream::new(&mut session, &mut socket);
            drop(std::io::Write::write_all(&mut link, b"never read"));
        }

        let refused = listening
            .join()
            .expect("the door's thread")
            .expect_err("a connection that proves nothing is refused");
        // Refused by the transport, inside the handshake — the earliest place a
        // refusal can happen, and before a single frame was parsed.
        assert!(matches!(refused, Error::Transport(_)), "{refused}");
    }
}
