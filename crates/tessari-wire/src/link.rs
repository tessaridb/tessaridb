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
//! to [`Peers::greet`]; a node that accepts peers continuously is the wave that
//! has something for them to say.

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
    pub fn greet(&self, mine: &Hello) -> Result<Hello> {
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
        Ok(said)
    }
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
) -> Result<Hello> {
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
    let mut link = rustls::Stream::new(&mut session, &mut socket);
    say(&mut link, said)?;
    hear(&mut link)
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
        None => Err(Error::UnknownFrame { tag }),
    }
}

#[cfg(test)]
mod tests {
    use super::{Credential, Peers, call};
    use crate::credential::names;
    use crate::error::Error;
    use crate::peer::{Hello, Purpose};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::net::TcpStream;
    use std::sync::Arc;
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
        let listening = std::thread::spawn(move || peers.greet(&mine));

        let theirs = call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
        )
        .expect("a peer that proved itself is answered");

        let heard = listening
            .join()
            .expect("the door's thread")
            .expect("the door admits a peer credential naming the greeter");
        // Each end learned the other's facts, and neither learned them from a
        // certificate: the epoch and the tail are in the frame because a
        // credential outlives both.
        assert_eq!(heard.node, THERE);
        assert_eq!(heard.epoch, Epoch::new(4));
        assert_eq!(heard.tail, Sequence::new(9));
        assert_eq!(theirs.node, HERE);
    }

    #[test]
    fn a_client_credential_on_the_peer_link_is_refused_on_its_purpose() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || peers.greet(&mine));

        // The id is perfectly correct. What is wrong is the link it was issued
        // for, which is the criterion's own sentence.
        drop(call(
            address,
            authority.issue(THERE, Purpose::Client),
            &authority.der(),
            HERE,
            &hello(THERE),
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
        let listening = std::thread::spawn(move || peers.greet(&mine));

        // Issued by the right authority, for the right link, for the wrong node.
        drop(call(
            address,
            authority.issue([3_u8; NODE_ID_LEN], Purpose::Peer),
            &authority.der(),
            HERE,
            &hello(THERE),
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

    #[test]
    fn a_connection_offering_no_credential_never_reaches_a_frame() {
        let authority = Authority::new();
        let (peers, mine) = door(&authority);
        let address = peers.address().expect("the door's address");
        let listening = std::thread::spawn(move || peers.greet(&mine));

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
