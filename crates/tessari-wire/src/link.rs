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
//! to [`Peers::greet`], and one ballot rides it — a node that accepts peers
//! continuously, and holds the connection open between rounds, is a later wave.
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

use crate::credential;
use crate::error::{Error, Result};
use crate::frame;
use crate::grant::{Ballot, Vote, Voter};
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

    /// Take one peer, prove who it is, and answer with `mine`.
    ///
    /// The order is deliberate and is the module's whole argument: the
    /// credential is settled first, the greeting is read second, and this node
    /// says what it holds only after both. A node that greeted first would be
    /// telling an unproven stranger its epoch and how far its log reaches.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unidentified`] when nothing was presented,
    /// [`Error::CredentialNamesAnother`] when what was presented does not name
    /// the node the greeting claims, and [`Error::NotAPeerCredential`] when it
    /// names that node for the client link instead of this one.
    pub fn greet(&self, mine: &Hello, voter: &mut Voter) -> Result<Met> {
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
        admit(Some(&presented), &said)?;
        say(&mut link, mine)?;

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
                Some(PeerFrame::Ballot) => {
                    let asked = Ballot::decode(&body)?;
                    let vote = voter.asked(&asked, std::time::Instant::now());
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

/// What one served connection produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Met {
    /// What the peer said it holds.
    pub said: Hello,
    /// How this node voted, if the peer asked for anything.
    pub voted: Option<Vote>,
}

/// Reach the peer `at` on `address`, and exchange greetings.
///
/// # Errors
///
/// Returns [`Error::Transport`] when the node reached does not hold a peer
/// credential for `at` — the handshake refuses it, which is why this side needs
/// no admission rule of its own.
pub fn call(
    address: impl ToSocketAddrs,
    mine: Credential,
    authority: &CertificateDer<'_>,
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Option<&Ballot>,
) -> Result<(Hello, Option<Vote>)> {
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

/// The greeting and the ballot, on a session that is already open.
fn exchange(
    session: &mut ClientConnection,
    socket: &mut TcpStream,
    said: &Hello,
    asking: Option<&Ballot>,
) -> Result<(Hello, Option<Vote>)> {
    let mut link = rustls::Stream::new(session, socket);
    say(&mut link, said)?;
    let heard = hear(&mut link)?;

    let Some(ballot) = asking else {
        return Ok((heard, None));
    };
    frame::write_tagged(&mut link, PeerFrame::Ballot.tag(), &ballot.encode())?;
    let Some((tag, body)) = frame::read_tagged(&mut link)? else {
        return Err(Error::Truncated);
    };
    match PeerFrame::from_tag(tag) {
        Some(PeerFrame::Vote) => Ok((heard, Some(Vote::decode(&body)?))),
        Some(_) => Err(Error::OutOfTurn { tag }),
        None => Err(Error::UnknownFrame { tag }),
    }
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
mod tests {
    use super::{Credential, Met, Peers, Result, call};
    use crate::credential::names;
    use crate::error::Error;
    use crate::grant::{Ballot, Refused, Round, Vote, Voter};
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
    struct Authority {
        certificate: rcgen::Certificate,
        key: rcgen::KeyPair,
    }

    impl Authority {
        fn new() -> Self {
            let mut params =
                rcgen::CertificateParams::new(Vec::new()).expect("an authority's parameters");
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let key = rcgen::KeyPair::generate().expect("an authority's key");
            let certificate = params.self_signed(&key).expect("a self-signed authority");
            Self { certificate, key }
        }

        fn der(&self) -> CertificateDer<'static> {
            CertificateDer::from(self.certificate.der().to_vec())
        }

        /// Issue a credential naming `node` for `purpose`.
        fn issue(&self, node: [u8; NODE_ID_LEN], purpose: Purpose) -> Credential {
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

    fn identity(node: [u8; NODE_ID_LEN]) -> NodeIdentity {
        NodeIdentity::alone(node)
    }

    fn hello(node: [u8; NODE_ID_LEN]) -> Hello {
        Hello::about(&identity(node), Epoch::new(4), Sequence::new(9))
    }

    /// A voter that has been up long enough to have outlived anything it could
    /// have granted before a restart — otherwise every door in these tests
    /// would refuse on the rule that has nothing to do with what is being
    /// tested.
    fn settled() -> Voter {
        Voter::started_at(
            std::time::Instant::now()
                .checked_sub(tessari_storage::LEASE_TTL)
                .expect("this machine has been up for ten seconds"),
        )
    }

    const HERE: [u8; NODE_ID_LEN] = [1_u8; NODE_ID_LEN];
    const THERE: [u8; NODE_ID_LEN] = [2_u8; NODE_ID_LEN];

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
        let listening = std::thread::spawn(move || peers.greet(&mine, &mut settled()));

        let theirs = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            None,
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
    fn a_client_credential_on_the_peer_link_is_refused_on_its_purpose() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || peers.greet(&mine, &mut settled()));

        // The id is perfectly correct. What is wrong is the link it was issued
        // for, which is the criterion's own sentence.
        drop(call(
            address,
            authority.issue(THERE, Purpose::Client),
            &authority.der(),
            HERE,
            &hello(THERE),
            None,
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
        let listening = std::thread::spawn(move || peers.greet(&mine, &mut settled()));

        // Issued by the right authority, for the right link, for the wrong node.
        drop(call(
            address,
            authority.issue([3_u8; NODE_ID_LEN], Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
            None,
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
    fn voting(
        authority: &Authority,
        id: [u8; NODE_ID_LEN],
        mut voter: Voter,
    ) -> (SocketAddr, JoinHandle<Result<Met>>) {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(id, Purpose::Peer),
            &authority.der(),
        )
        .expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let mine = hello(id);
        let answering = std::thread::spawn(move || peers.greet(&mine, &mut voter));
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
            Some(&ballot),
        )
        .expect("a peer that proved itself may ask");

        assert_eq!(vote, Some(Vote::Granted));
        let met = answering
            .join()
            .expect("the door's thread")
            .expect("served");
        assert_eq!(met.voted, Some(Vote::Granted), "both ends saw one answer");
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

        // One voter, two rounds, and the second arrives while the first grant
        // is unmistakably still alive.
        let mine = hello(HERE);
        let answering = std::thread::spawn(move || {
            let mut voter = settled();
            let first = peers.greet(&mine, &mut voter);
            let second = peers.greet(&mine, &mut voter);
            (first, second)
        });

        let ask = |epoch: u64| {
            call(
                address,
                authority.issue(THERE, Purpose::Peer),
                &authority.der(),
                HERE,
                &hello(THERE),
                Some(&Ballot {
                    epoch: Epoch::new(epoch),
                    candidate: THERE,
                }),
            )
            .expect("a peer that proved itself may ask")
            .1
        };

        assert_eq!(ask(1), Some(Vote::Granted));
        let refused = ask(2).expect("a vote came back");
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
                Some(&round.ballot()),
            )
            .expect("every door is up");
            held = round.counts(*id, vote.expect("a door that was asked answers"));
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
                Some(&round.ballot()),
            )
            .expect("every door is up and answering");
            let vote = vote.expect("a door that was asked answers");
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
            Some(&round.ballot()),
        )
        .expect("the one door that is up answers");
        assert_eq!(
            round.counts(alive, vote.expect("it answered")),
            None,
            "one of three is not a majority"
        );

        let reached = call(
            vanished,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            [21_u8; NODE_ID_LEN],
            &hello(THERE),
            Some(&round.ballot()),
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
                Some(&round.ballot()),
            )
            .expect("every door is up");
            held = round.counts(*id, vote.expect("it answered"));
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
                Some(&renewal.ballot()),
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
    fn a_connection_offering_no_credential_never_reaches_a_frame() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || peers.greet(&mine, &mut settled()));

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
