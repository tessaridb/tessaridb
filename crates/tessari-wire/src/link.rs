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
//! here issues a certificate; what is presented and what is refused is read
//! from [`PeerKeys`] at each handshake, which is where rotation and revocation
//! live (ADR-0108 D6). One connection is served per call
//! to [`Peers::greet`], and ONE follow-up rides it — a ballot or a collection,
//! never both, which is why [`Ask`] is an enum rather than two optional
//! arguments. A node serving peers continuously does it on the runtime, one
//! task per connection (`door.rs`); holding a connection open between rounds
//! is a later wave.
//!
//! Nothing decides **when** to stand for leadership or how often to renew. A
//! round is opened by its caller. What this module owes is that the ballot can
//! travel at all and that a refusal arrives as the refusal it was.

use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConnection, ServerConfig, ServerConnection};

use tessari_constants::GREETING_SECONDS;
use tessari_encoding::NODE_ID_LEN;

use crate::collection::{Collect, Collected, Origin};
use crate::credential;
use crate::error::{Error, Result};
use crate::frame;
use crate::gathering::{Gather, Page, Ungathered};
use crate::grant::{Ballot, Deciding, Refused, Vote};
use crate::keys::PeerKeys;
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
    pub(crate) listener: TcpListener,
    pub(crate) settings: Arc<ServerConfig>,
    /// The same keys the settings judge a handshake by, asked again while a
    /// held stream is open.
    pub(crate) keys: PeerKeys,
}

impl Peers {
    /// Open the peer door at `address`, answering with whatever `keys` holds
    /// at each handshake and admitting what its authority issued and its
    /// revocations do not name.
    ///
    /// # Errors
    ///
    /// Returns the socket's own failure.
    pub fn bind(address: impl ToSocketAddrs, keys: &PeerKeys) -> Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(address)?,
            settings: keys.door(),
            keys: keys.clone(),
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
        socket.set_nodelay(true)?;
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
            // A copy is the one follow-up answered by more than one frame
            // (ADR-0094 D3), so it is written here rather than by `answering`.
            Some((tag, _)) if tag == PeerFrame::State.tag() => {
                log.copied(said.node, &mut |tag, body| {
                    frame::write_tagged(&mut link, tag, &body)
                })
                .or_else(|why| refusal_frame(&mut link, why))?;
                None
            }
            Some((tag, body)) => {
                let (tag, reply, voted) = answering(tag, &body, &said, &mine, voter, log)?;
                frame::write_tagged(&mut link, tag, &reply)?;
                voted
            }
        };
        Ok(Met {
            said,
            voted,
            presented: shown.as_ref().map_or([0; 32], credential::digest),
        })
    }
}

/// A copy's refusal, written as the frame it crosses the wire as: the
/// subscription's, for a node nobody subscribed, or the failure itself.
pub(crate) fn refusal_frame(link: &mut impl std::io::Write, why: Error) -> Result<()> {
    match why {
        Error::Unsubscribed => frame::write_tagged(link, PeerFrame::Unsubscribed.tag(), &[]),
        other => Err(other),
    }
}

/// The one frame that answers a peer's follow-up, and the vote it cast if it was a ballot.
///
/// The door's decision, in one place for both doors: the synchronous
/// [`Peers::greet`] and the door that serves on the runtime (`door.rs`) write
/// whatever this returns. `said` is the greeting the handshake-proved peer sent
/// on this connection and `mine` is what this node answered it with; both are
/// what a ballot is judged on, for the reasons given where they are used.
///
/// # Errors
///
/// [`Error::NotItsOwnBallot`] when a ballot names a candidate other than the
/// proved peer, [`Error::OutOfTurn`] or [`Error::UnknownFrame`] for anything
/// that is not a follow-up, a malformed body, and a store failure other than the
/// refusals that cross the wire as frames.
pub(crate) fn answering(
    tag: u8,
    body: &[u8],
    said: &Hello,
    mine: &Hello,
    voter: &Deciding,
    log: &dyn Origin,
) -> Result<(u8, Vec<u8>, Option<Vote>)> {
    match PeerFrame::from_tag(tag) {
        Some(PeerFrame::Collect) => {
            let asked = Collect::decode(body)?;
            // The follower names itself by the id the handshake proved,
            // never by one it writes into a frame — so a peer cannot
            // record somebody else's progress, and the per-follower lag
            // report stays per follower.
            match log.collected(said.node, asked) {
                Ok(collected) => Ok((PeerFrame::Collected.tag(), collected.encode(), None)),
                // A refusal crosses the wire as the refusal it was. The
                // alternative is closing the socket, which reaches the
                // other end as a truncated conversation and sends
                // whoever reads it to look for a network fault.
                Err(Error::Uncollectable { from }) => {
                    let mut refused = Vec::with_capacity(8);
                    frame::put_u64(&mut refused, from);
                    Ok((PeerFrame::Uncollectable.tag(), refused, None))
                }
                // The same rule one step out: a node nobody
                // subscribed learns that, rather than watching its
                // socket close and reading it as a network fault.
                Err(Error::Unsubscribed) => Ok((PeerFrame::Unsubscribed.tag(), Vec::new(), None)),
                Err(why) => Err(why),
            }
        }
        // A shard's records, for a node holding part of its table
        // (G033). Its refusal crosses as a frame for the reason the
        // collection's two do.
        Some(PeerFrame::Gather) => {
            let asked = Gather::decode(body)?;
            match log.gathered(said.node, &asked) {
                Ok(page) => Ok((PeerFrame::Gathered.tag(), page.encode(), None)),
                Err(Error::NotGathered(why)) => {
                    Ok((PeerFrame::NotGathered.tag(), vec![why.byte()], None))
                }
                Err(why) => Err(why),
            }
        }
        // A sign-in try, against this store's own table — the cluster's
        // while this node leads the store line (ADR-0108 D5). Only a proven
        // member reaches here; it learns whether a name may try, nothing more.
        Some(PeerFrame::Attempt) => {
            let asked = crate::budget::Attempt::decode(body)?;
            let answer = u8::from(log.attempted(&asked));
            Ok((PeerFrame::Attempted.tag(), vec![answer], None))
        }
        // A join token, from the node the handshake proved — never a node the
        // frame names, so a member cannot spend a token for somebody else.
        Some(PeerFrame::Join) => {
            let token = <[u8; 32]>::try_from(body).map_err(|_| Error::Malformed)?;
            let bound = log.joined(said.node, &token)?;
            Ok((PeerFrame::Joined.tag(), vec![u8::from(bound)], None))
        }
        Some(PeerFrame::Ballot) => {
            let asked = Ballot::decode(body)?;
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
            // ADR-0098. A placed range's ballot is granted only to a node
            // this catalog places on it, so a node a placement was taken
            // from cannot renew there once this voter has applied the move.
            if asked.range != tessari_types::Reach::Store && !log.places(said.node, asked.range) {
                let vote = Vote::Refused(Refused::NotPlaced);
                return Ok((PeerFrame::Vote.tag(), vote.encode(), Some(vote)));
            }
            // On the ballot's own line (ADR-0082): a range ballot is
            // judged on the positions for that range, the store ballot on
            // the store's exactly as before. The candidate's from the
            // greeting it proved, which describes the range it stands for;
            // this voter's from what it holds of that line, which its own
            // greeting may not describe at all (Q-884).
            let held = log.reached_on(asked.range)?;
            let vote = voter.asked(
                &asked,
                std::time::Instant::now(),
                held.unwrap_or_else(|| mine.reached_on(asked.range)),
                said.reached_on(asked.range),
            );
            Ok((PeerFrame::Vote.tag(), vote.encode(), Some(vote)))
        }
        Some(_) => Err(Error::OutOfTurn { tag }),
        None => Err(Error::UnknownFrame { tag }),
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
    /// The SHA-256 of the certificate the peer presented, which a row's pinned
    /// fingerprint is compared with (ADR-0108 D9).
    pub presented: [u8; 32],
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
    /// One page of a shard's records, from that shard's leader (G033).
    Gather(&'a Gather),
    /// A request this node cannot answer, for a caller it verified (ADR-0108).
    Coordinate(&'a crate::coordination::Coordinate),
    /// A sign-in try asked about or reported to the store line's leader
    /// (ADR-0108 D5).
    Attempt(&'a crate::budget::Attempt),
    /// A join token, offered to the node that may bind this one's row
    /// (ADR-0108 D9).
    Join(&'a [u8; 32]),
}

/// What the other end answered with.
///
/// Paired with [`Ask`] on purpose: an answer of the wrong kind is a peer
/// speaking this protocol incorrectly and is refused as
/// [`Error::OutOfTurn`], rather than accepted because it happened to decode.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Answered {
    /// Nothing was asked, so nothing was answered.
    Nothing,
    /// How the peer voted.
    Voted(Vote),
    /// What the peer's log held.
    Collected(Collected),
    /// One page of a shard's records.
    Gathered(Page),
    /// The answer to a carried request.
    Coordinated(tessaridb::Coordinated),
    /// Whether the name may try now (`true` for a report).
    Attempted(bool),
    /// Whether a row now names the asker.
    Joined(bool),
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
    keys: &PeerKeys,
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Ask<'_>,
) -> Result<(Hello, Answered)> {
    call_within(
        address,
        (keys, keys.duplicate()),
        at,
        said,
        asking,
        Duration::from_secs(GREETING_SECONDS),
    )
}

/// [`call`], with the connect and every read and write bounded by `bound`
/// rather than by the greeting's deadline.
///
/// For a caller that has a deadline of its own — a ballot is worth nothing once
/// its round is over, and a member that accepts the connection and then says
/// nothing must cost the round no more than the round (G053 SG2b).
///
/// `mine` is the credential this dial presents, taken from `keys` by the
/// caller, so a caller that signs with the key as well signs and presents one
/// snapshot even when a rotation lands between the two.
///
/// # Errors
///
/// As [`call`], plus [`Error::Io`] when `bound` passes on any one step.
pub fn call_within(
    address: impl ToSocketAddrs,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Ask<'_>,
    bound: Duration,
) -> Result<(Hello, Answered)> {
    let (mut session, mut socket) = open_within(address, (keys, mine), at, bound)?;
    let exchanged = exchange(&mut session, &mut socket, said, asking);

    // Say goodbye properly even when the exchange failed. A TLS peer that just
    // drops the socket makes the other end's next read an error rather than an
    // end, so a caller that skipped this would leave every door it spoke to
    // reporting a fault it did not have.
    session.send_close_notify();
    drop(session.write_tls(&mut socket));
    exchanged
}

/// Reach the peer `at` on `address` and open the TLS session a conversation
/// rides, with every read and write bounded by the greeting's deadline.
pub(crate) fn open(
    address: impl ToSocketAddrs,
    keys: &PeerKeys,
    at: [u8; NODE_ID_LEN],
) -> Result<(ClientConnection, TcpStream)> {
    open_within(
        address,
        (keys, keys.duplicate()),
        at,
        Duration::from_secs(GREETING_SECONDS),
    )
}

/// [`open`], with the connect and every read and write bounded by `bound`.
fn open_within(
    address: impl ToSocketAddrs,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    bound: Duration,
) -> Result<(ClientConnection, TcpStream)> {
    let settings = keys.dialling(mine)?;
    let expected = credential::names(at, Purpose::Peer);
    let name = ServerName::try_from(expected).map_err(|why| Error::Transport(why.to_string()))?;
    let session = ClientConnection::new(Arc::new(settings), name)
        .map_err(|why| Error::Transport(why.to_string()))?;

    let socket = connect(address, bound)?;
    // A zero would mean *no timeout at all* to the socket, the opposite of what
    // a spent deadline asks for, so the least a step may have is a millisecond.
    let bound = Some(bound.max(Duration::from_millis(1)));
    socket.set_read_timeout(bound)?;
    socket.set_write_timeout(bound)?;
    Ok((session, socket))
}

/// Open the connection a call rides, giving up after `bound` per address.
///
/// The reads and writes were always bounded and the connect was not, so a peer
/// whose host drops SYNs held the calling round for the kernel's own connect
/// limit — and the rounds dial their peers one after another (Q-836). Each
/// resolved address is tried in turn, as `TcpStream::connect` does.
fn connect(address: impl ToSocketAddrs, bound: Duration) -> Result<TcpStream> {
    let mut failed = None;
    for at in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&at, bound) {
            Ok(socket) => {
                socket.set_nodelay(true)?;
                return Ok(socket);
            }
            Err(why) => failed = Some(why),
        }
    }
    Err(failed
        .unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the peer's address resolved to nothing",
            )
        })
        .into())
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
        Ask::Join(token) => {
            frame::write_tagged(&mut link, PeerFrame::Join.tag(), token.as_slice())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Joined) => match body.as_slice() {
                    [bound] => Ok((heard, Answered::Joined(*bound == 1))),
                    _ => Err(Error::Malformed),
                },
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Attempt(asked) => {
            frame::write_tagged(&mut link, PeerFrame::Attempt.tag(), &asked.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Attempted) => match body.as_slice() {
                    [permitted] => Ok((heard, Answered::Attempted(*permitted == 1))),
                    _ => Err(Error::Malformed),
                },
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Coordinate(request) => {
            frame::write_tagged(&mut link, PeerFrame::Coordinate.tag(), &request.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Coordinated) => Ok((
                    heard,
                    Answered::Coordinated(crate::coordination::decode_answer(&body)?),
                )),
                Some(PeerFrame::NotCoordinated) => Err(Error::NotCoordinated(
                    String::from_utf8_lossy(&body).into_owned(),
                )),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Gather(gather) => {
            frame::write_tagged(&mut link, PeerFrame::Gather.tag(), &gather.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Gathered) => Ok((heard, Answered::Gathered(Page::decode(&body)?))),
                // The refusal as the leader sent it, for the reason the two
                // collection refusals above cross the wire as frames.
                Some(PeerFrame::NotGathered) => {
                    let why = body.first().copied().ok_or(Error::Malformed)?;
                    Err(Error::NotGathered(Ungathered::from_byte(why)?))
                }
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
pub(crate) fn say(link: &mut impl std::io::Write, hello: &Hello) -> Result<()> {
    frame::write_tagged(link, PeerFrame::Hello.tag(), &hello.encode())
}

/// Take one greeting off the link, and refuse anything else.
pub(crate) fn hear(link: &mut impl std::io::Read) -> Result<Hello> {
    let Some((tag, body)) = frame::read_tagged(link)? else {
        return Err(Error::Truncated);
    };
    greeting(tag, &body)
}

/// A frame that has to be a greeting, and is refused as anything else.
///
/// Shared with the door on the runtime, which reads the frame its own way.
pub(crate) fn greeting(tag: u8, body: &[u8]) -> Result<Hello> {
    match PeerFrame::from_tag(tag) {
        Some(PeerFrame::Hello) => Hello::decode(body),
        Some(_) => Err(Error::OutOfTurn { tag }),
        None => Err(Error::UnknownFrame { tag }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Answered, Ask, Credential, Met, PeerKeys, Peers, Result, call};
    use crate::collection::{Collect, Collected, NoLog, Origin};
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

        /// A handle on a credential naming `node` for `purpose`.
        pub(crate) fn keys(&self, node: [u8; NODE_ID_LEN], purpose: Purpose) -> PeerKeys {
            keys(self.issue(node, purpose), &self.der()).expect("a credential the authority issued")
        }

        /// Issue a credential naming `node` for `purpose`.
        pub(crate) fn issue(&self, node: [u8; NODE_ID_LEN], purpose: Purpose) -> Credential {
            self.named(&names(node, purpose))
        }

        /// A credential naming `node` for `purpose` whose validity ended in 2001.
        pub(crate) fn expired(&self, node: [u8; NODE_ID_LEN], purpose: Purpose) -> Credential {
            let mut params = rcgen::CertificateParams::new(vec![names(node, purpose)])
                .expect("a leaf's parameters");
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
            self.signed(params)
        }

        fn named(&self, name: &str) -> Credential {
            let params =
                rcgen::CertificateParams::new(vec![name.to_owned()]).expect("a leaf's parameters");
            self.signed(params)
        }

        fn signed(&self, params: rcgen::CertificateParams) -> Credential {
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

    /// A handle on `mine`, built fresh — what one door or one dial held before
    /// credentials were shared (ADR-0108 D6).
    pub(crate) fn keys(mine: Credential, authority: &CertificateDer<'_>) -> Result<PeerKeys> {
        PeerKeys::new(mine, authority.clone().into_owned())
    }

    /// A door answering with `mine`.
    pub(crate) fn bind_with(
        address: impl std::net::ToSocketAddrs,
        mine: Credential,
        authority: &CertificateDer<'_>,
    ) -> Result<Peers> {
        Peers::bind(address, &keys(mine, authority)?)
    }

    /// One dial presenting `mine`.
    pub(crate) fn call_with(
        address: impl std::net::ToSocketAddrs,
        mine: Credential,
        authority: &CertificateDer<'_>,
        at: [u8; NODE_ID_LEN],
        said: &Hello,
        asking: Ask<'_>,
    ) -> Result<(Hello, Answered)> {
        call(address, &keys(mine, authority)?, at, said, asking)
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
            None,
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
        let peers = bind_with(
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

        let theirs = call_with(
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
                        None,
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

        let theirs = call_with(
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

        let failure = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Records(Collect {
                home: tessari_types::Reach::Store,
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
        drop(call_with(
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
        drop(call_with(
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
        let peers = bind_with(
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
                range: tessari_types::Reach::Store,
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
            range: tessari_types::Reach::Store,
        };
        let (_, vote) = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&ballot),
        )
        .expect("a peer that proved itself may ask");

        assert_eq!(
            voted(&vote),
            Some(Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            })
        );
        let met = answering
            .join()
            .expect("the door's thread")
            .expect("served");
        assert_eq!(
            met.voted,
            Some(Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            }),
            "both ends saw one answer"
        );
    }

    /// A greeting from a node whose log stops short of [`LEVEL`].
    fn falling_behind(node: [u8; NODE_ID_LEN]) -> Hello {
        Hello::about(
            &identity(node),
            Epoch::new(4),
            Sequence::new(LEVEL.tail.get().saturating_sub(3)),
            LEVEL.leadership,
            Some(core::time::Duration::ZERO),
            None,
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

        let (_, vote) = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &falling_behind(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(12),
                candidate: THERE,
                range: tessari_types::Reach::Store,
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
        let peers = bind_with(
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

        let _ = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(1),
                candidate: HERE,
                range: tessari_types::Reach::Store,
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
        let peers = bind_with(
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

        let refused = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            Ask::Ballot(&Ballot {
                epoch: Epoch::new(2),
                candidate: THERE,
                range: tessari_types::Reach::Store,
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
            let (_, vote) = call_with(
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
            let (_, vote) = call_with(
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
        let unreachable = bind_with(
            "127.0.0.1:0",
            authority.issue([21_u8; NODE_ID_LEN], Purpose::Peer),
            &authority.der(),
        )
        .expect("a door, briefly");
        let vanished = unreachable.address().expect("its address");
        drop(unreachable);

        let mut round = Round::opened(Epoch::new(9), THERE, 3);
        let (_, vote) = call_with(
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

        let reached = call_with(
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
        // arithmetic it runs is the same one the shipped lease runs.
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
            let (_, vote) = call_with(
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
            let reached = call_with(
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

        // Past the fence, which is `ttl - GUARD` = 400 ms, and short of the
        // expiry by half the guard. That gap is the whole point: the holder
        // stops writing while the cluster still may not reassign. Aimed at the
        // middle of the guard from the instant the lease was taken, because the
        // guard is 150 ms and a fixed sleep after the writes above would spend
        // part of it on them (G053 SG2b).
        let middle = Duration::from_millis(400)
            .saturating_add(tessari_storage::LEASE_GUARD.checked_div(2).expect("halves"));
        std::thread::sleep(middle.saturating_sub(taken.elapsed()));
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
            .counts(
                voter,
                Vote::Granted {
                    hold: tessari_storage::LEASE_TTL,
                },
            )
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
            .counts(
                voter,
                Vote::Granted {
                    hold: tessari_storage::LEASE_TTL,
                },
            )
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

    #[test]
    fn a_range_ballot_is_judged_on_both_greetings_positions_for_that_range() {
        // G032 S3.2. The voter stands for shard 2 and its own log of it reaches
        // further than the candidate's: a ballot on shard 2 is refused as behind,
        // while the same candidate's store ballot -- level on the store -- and
        // its ballot on shard 3, which the voter never stood for, are granted.
        use crate::peer::Line;
        use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
        let shard = |n: u32| {
            Reach::Shard(
                NamespaceId::new(1),
                DatabaseId::new(1),
                TableId::new(1),
                ShardId::new(n),
            )
        };
        let line = |n: u32, tail: u64| Line {
            range: shard(n),
            leading: Epoch::ZERO,
            tail: Sequence::new(tail),
            tail_leadership: Epoch::new(2),
        };
        let authority = Authority::new();
        let (peers, mut mine) = door(&authority);
        mine.line = Some(line(2, 40));
        let address = peers.address().expect("the door's address");
        let deciding = Deciding::holding(settled());
        let answering = std::thread::spawn(move || {
            (0..3)
                .map(|_| {
                    peers.greet(
                        || Ok(mine),
                        &HERE,
                        &deciding,
                        &Placing(vec![shard(2), shard(3)]),
                    )
                })
                .collect::<Vec<_>>()
        });
        let mut candidate = hello(THERE);
        candidate.line = Some(line(2, 3));
        let ask = |range: Reach| {
            let ballot = Round::opened(Epoch::new(1), THERE, 3).over(range).ballot();
            let (_, answered) = call_with(
                address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                HERE,
                &candidate,
                Ask::Ballot(&ballot),
            )
            .expect("the door is up");
            voted(&answered).expect("a door that was asked answers")
        };
        assert!(
            matches!(ask(shard(2)), Vote::Refused(Refused::LogBehind { tail, .. }) if tail == Sequence::new(40)),
            "behind on the range it asked for"
        );
        assert_eq!(
            ask(Reach::Store),
            Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            },
            "level on the store"
        );
        assert_eq!(
            ask(shard(3)),
            Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            },
            "the voter never stood for shard 3"
        );
        drop(answering.join().expect("the door's thread"));
    }

    /// A door with no log whose catalog places `THERE` on the ranges it holds.
    struct Placing(Vec<tessari_types::Reach>);

    impl Origin for Placing {
        fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
            NoLog.collected(follower, asked)
        }

        fn gathered(
            &self,
            asker: [u8; NODE_ID_LEN],
            asked: &crate::gathering::Gather,
        ) -> Result<crate::gathering::Page> {
            NoLog.gathered(asker, asked)
        }

        fn copied(
            &self,
            follower: [u8; NODE_ID_LEN],
            write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
        ) -> Result<()> {
            NoLog.copied(follower, write)
        }

        fn places(&self, candidate: [u8; NODE_ID_LEN], range: tessari_types::Reach) -> bool {
            candidate == THERE && self.0.contains(&range)
        }
    }

    /// A door whose catalog places `THERE` on one range, and whose store holds
    /// that range's line to a position its greeting does not mention — the
    /// former leader after a move, or a follower that collected the line.
    struct Former(tessari_types::Reach, crate::grant::Reached);

    impl Origin for Former {
        fn collected(&self, follower: [u8; NODE_ID_LEN], asked: Collect) -> Result<Collected> {
            NoLog.collected(follower, asked)
        }

        fn gathered(
            &self,
            asker: [u8; NODE_ID_LEN],
            asked: &crate::gathering::Gather,
        ) -> Result<crate::gathering::Page> {
            NoLog.gathered(asker, asked)
        }

        fn copied(
            &self,
            follower: [u8; NODE_ID_LEN],
            write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
        ) -> Result<()> {
            NoLog.copied(follower, write)
        }

        fn places(&self, candidate: [u8; NODE_ID_LEN], range: tessari_types::Reach) -> bool {
            candidate == THERE && range == self.0
        }

        fn reached_on(&self, range: tessari_types::Reach) -> Result<Option<crate::grant::Reached>> {
            Ok((range == self.0).then_some(self.1))
        }
    }

    #[test]
    fn a_voter_holding_a_line_it_no_longer_leads_refuses_a_candidate_behind_it() {
        // Q-884. A move took the placement from this voter, so its greeting
        // names no line; its store still holds the line's log to 40, written
        // under leadership 2. A candidate at 3 must be refused as behind — the
        // greeting alone would read this voter as empty there and grant it,
        // and the candidate's leadership would continue the line from 3.
        use crate::peer::Line;
        use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
        let shard = Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            TableId::new(1),
            ShardId::new(2),
        );
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        assert!(mine.line.is_none(), "the voter greets with no line");
        let address = peers.address().expect("the door's address");
        let deciding = Deciding::holding(settled());
        let held = crate::grant::Reached {
            leadership: Epoch::new(2),
            tail: Sequence::new(40),
        };
        let answering = std::thread::spawn(move || {
            peers.greet(|| Ok(mine), &HERE, &deciding, &Former(shard, held))
        });
        let mut candidate = hello(THERE);
        candidate.line = Some(Line {
            range: shard,
            leading: Epoch::ZERO,
            tail: Sequence::new(3),
            tail_leadership: Epoch::new(2),
        });
        let ballot = Round::opened(Epoch::new(3), THERE, 3).over(shard).ballot();
        let (_, answered) = call_with(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &candidate,
            Ask::Ballot(&ballot),
        )
        .expect("the door is up");
        assert!(
            matches!(
                voted(&answered),
                Some(Vote::Refused(Refused::LogBehind { tail, .. })) if tail == Sequence::new(40)
            ),
            "judged on the line the voter holds: {:?}",
            voted(&answered)
        );
        drop(answering.join().expect("the door's thread"));
    }

    #[test]
    fn a_range_ballot_from_a_candidate_not_placed_on_it_is_refused() {
        // ADR-0098. Once this voter's catalog no longer places the candidate on
        // shard 3, the candidate cannot renew there; its store ballot and its
        // ballot on the shard it is placed on are judged as before.
        use tessari_types::{DatabaseId, NamespaceId, Reach, ShardId, TableId};
        let shard = |n: u32| {
            Reach::Shard(
                NamespaceId::new(1),
                DatabaseId::new(1),
                TableId::new(1),
                ShardId::new(n),
            )
        };
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let deciding = Deciding::holding(settled());
        let placing = Placing(vec![shard(2)]);
        let answering = std::thread::spawn(move || {
            (0..3)
                .map(|_| peers.greet(|| Ok(mine), &HERE, &deciding, &placing))
                .collect::<Vec<_>>()
        });
        let candidate = hello(THERE);
        let ask = |range: Reach| {
            let ballot = Round::opened(Epoch::new(1), THERE, 3).over(range).ballot();
            let (_, answered) = call_with(
                address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                HERE,
                &candidate,
                Ask::Ballot(&ballot),
            )
            .expect("the door is up");
            voted(&answered).expect("a door that was asked answers")
        };
        assert_eq!(ask(shard(3)), Vote::Refused(Refused::NotPlaced));
        assert_eq!(
            ask(shard(2)),
            Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            },
            "placed on shard 2"
        );
        assert_eq!(
            ask(Reach::Store),
            Vote::Granted {
                hold: tessari_storage::LEASE_TTL
            },
            "the store is not placed"
        );
        drop(answering.join().expect("the door's thread"));
    }

    /// Fill the queue of a listener that never accepts, so the next connect
    /// gets no answer at all — the black-holed peer, without leaving loopback.
    ///
    /// A full accept queue drops a SYN rather than refusing it (measured on
    /// macOS: the 129th connect to a never-accepting listener times out).
    fn a_peer_that_answers_no_syn() -> (std::net::TcpListener, Vec<TcpStream>, SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = listener.local_addr().expect("its address");
        let mut held = Vec::new();
        while held.len() < 1024 {
            match TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
        assert!(
            held.len() < 1024,
            "a thousand connects to a listener that never accepts all completed"
        );
        (listener, held, address)
    }

    #[test]
    fn a_peer_that_never_answers_the_connect_costs_the_deadline_and_no_more() {
        let (_listener, _held, address) = a_peer_that_answers_no_syn();
        // A deadline shorter than the operating system's own: this loopback
        // gives up on its own after about eight seconds, a remote black hole
        // only after the kernel's connect limit (`keepinit`, 75 s here).
        let bound = Duration::from_secs(1);
        let began = std::time::Instant::now();
        let failed = super::connect(address, bound);
        let waited = began.elapsed();
        assert!(failed.is_err(), "nobody answered, so nothing connected");
        assert!(
            waited < bound.saturating_mul(3),
            "a black-holed peer held the connect for {waited:?}; the deadline is {bound:?}"
        );
    }
}
