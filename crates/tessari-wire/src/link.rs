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
            listener: tessari_serve::listen(address)?,
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
    /// One record of a transaction across leaders, for its range's leader to
    /// write (ADR-0112).
    Across(&'a crate::across::Carried),
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
    /// What a range's leader wrote for a transaction across leaders.
    Across(tessari_session::AcrossAnswer),
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
pub(crate) fn open_within(
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
        Ask::Across(carried) => {
            frame::write_tagged(&mut link, PeerFrame::Across.tag(), &carried.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::AcrossDone) => Ok((
                    heard,
                    Answered::Across(
                        tessari_session::AcrossAnswer::decode(&body)
                            .map_err(|_| Error::Malformed)?,
                    ),
                )),
                // The leader's refusal — its kind, then its words — which the
                // coordinator treats as *not prepared* and answers its caller
                // by the kind (Q-924).
                Some(PeerFrame::NotAcross) => Err(tessari_session::PartRefused::decode(&body)
                    .map_or(Error::Malformed, Error::RefusedAcross)),
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
pub(crate) mod tests;
