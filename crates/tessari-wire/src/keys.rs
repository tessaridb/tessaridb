//! This node's side of the peer link, held once and shared (ADR-0108 D6).
//!
//! # Why a handle and not a value
//!
//! The credential used to be copied into everything that dials — the rounds,
//! the coordinator, the gatherer, the sign-in budget — and into the door. A
//! copy cannot be replaced: a node whose certificate was renewed on disk kept
//! presenting the old one from every copy until it restarted, and a restart is
//! exactly what a rotation should not cost.
//!
//! So every holder holds this instead, and reads the credential **once per
//! connection**: a dial takes a snapshot, the door resolves one per handshake.
//! A replacement is therefore seen by the next connection and by no open one,
//! which finishes on the credential it started with.
//!
//! # Revocation lives here too
//!
//! A revoked certificate must be refused in both directions — at the door,
//! when a peer presents it, and at the dial, when the node reached presents it
//! — so both verifiers read the same set. The set is the catalog's
//! (`REVOKE CERTIFICATE`), handed in by whoever reads the catalog; this module
//! decides nothing about it beyond refusing what it holds.
//!
//! # What is fixed
//!
//! The authority. A new root is a change of who the cluster is, and rotating
//! one needs an overlap in which both are trusted; that is a restart with two
//! roots planned for, not a file that changed.
//!
//! Both locks are `std` mutexes held only to clone an `Arc` — never across a
//! handshake, a read or a write — so a reload never waits on a connection.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, RootCertStore,
    ServerConfig, SignatureScheme,
};

use crate::credential::{fingerprint, names, valid_for};
use crate::error::{Error, Result};
use crate::link::Credential;
use crate::peer::Purpose;
use tessari_encoding::NODE_ID_LEN;

/// The fingerprints a node refuses, as [`crate::fingerprint`] prints them.
pub type Revoked = BTreeSet<String>;

/// The nodes a cluster removed, refused whatever certificate they present.
pub type Removed = BTreeSet<[u8; NODE_ID_LEN]>;

/// What this node shows its peers, whom it trusts, and whom it no longer does.
///
/// Cloning shares the handle; it never copies the key.
#[derive(Clone)]
pub struct PeerKeys {
    held: Arc<Held>,
}

struct Held {
    shown: Mutex<Arc<Shown>>,
    revoked: Mutex<Arc<Revoked>>,
    removed: Mutex<Arc<Removed>>,
    authority: CertificateDer<'static>,
    clients: Arc<dyn ClientCertVerifier>,
    servers: Arc<WebPkiServerVerifier>,
}

/// One credential, and the form a handshake signs with.
struct Shown {
    credential: Credential,
    certified: Arc<CertifiedKey>,
}

impl std::fmt::Debug for PeerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerKeys")
            .field("shown", &self.fingerprint())
            .field("revoked", &self.revoked().len())
            .finish_non_exhaustive()
    }
}

impl PeerKeys {
    /// Hold `mine`, trusting exactly `authority`.
    ///
    /// One root and not a platform store: a cluster's peers are issued by the
    /// cluster, and a link that would also accept a certificate from any public
    /// authority is a link whose membership is whatever the operating system
    /// was shipped believing.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the authority is not a usable root, or when
    /// the key cannot sign or is not the key of the chain's first certificate.
    pub fn new(mine: Credential, authority: CertificateDer<'static>) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        roots.add(authority.clone()).map_err(transport)?;
        let roots = Arc::new(roots);
        let clients = WebPkiClientVerifier::builder(Arc::clone(&roots))
            .build()
            .map_err(transport)?;
        let servers = WebPkiServerVerifier::builder(roots)
            .build()
            .map_err(transport)?;
        Ok(Self {
            held: Arc::new(Held {
                shown: Mutex::new(Arc::new(Shown::of(mine)?)),
                revoked: Mutex::new(Arc::new(Revoked::new())),
                removed: Mutex::new(Arc::new(Removed::new())),
                authority,
                clients,
                servers,
            }),
        })
    }

    /// Present `mine` from the next connection on.
    ///
    /// Checked whole before anything changes, so a refused replacement leaves
    /// the node presenting what it presented before — a half-written file read
    /// mid-rotation costs one refused reload, never the node's identity.
    ///
    /// # Errors
    ///
    /// As [`Self::new`], for the credential.
    pub fn replace(&self, mine: Credential) -> Result<()> {
        let shown = Arc::new(Shown::of(mine)?);
        *self
            .held
            .shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = shown;
        Ok(())
    }

    /// Refuse exactly `fingerprints` from the next handshake on, in both
    /// directions.
    ///
    /// The whole set rather than additions, because the catalog is the list
    /// and this is a copy of it: a set handed in whole cannot drift from it.
    pub fn refuse(&self, fingerprints: Revoked) {
        *self
            .held
            .revoked
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(fingerprints);
    }

    /// Refuse every certificate naming one of `nodes` from the next handshake
    /// on, in both directions (ADR-0108 D9).
    ///
    /// By the name the certificate carries rather than by its fingerprint,
    /// because a removed node may hold any number of still-valid certificates
    /// and the removal is of the node.
    pub fn refuse_nodes(&self, nodes: Removed) {
        *self
            .held
            .removed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(nodes);
    }

    /// The nodes refused now.
    #[must_use]
    pub fn refusing_nodes(&self) -> Removed {
        Removed::clone(
            &self
                .held
                .removed
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// The fingerprints refused now.
    #[must_use]
    pub fn refusing(&self) -> Revoked {
        Revoked::clone(&self.revoked())
    }

    /// The fingerprint of the certificate this node presents now.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.shown()
            .credential
            .chain
            .first()
            .map(fingerprint)
            .unwrap_or_default()
    }

    /// The certificate presented now, without its key.
    #[must_use]
    pub fn leaf(&self) -> Option<CertificateDer<'static>> {
        self.shown().credential.chain.first().cloned()
    }

    /// A copy of the credential presented now, for one conversation.
    ///
    /// The copy is the snapshot: a replacement made while the conversation is
    /// open does not reach it.
    #[must_use]
    pub fn duplicate(&self) -> Credential {
        self.shown().credential.duplicate()
    }

    /// The one root peers are issued by.
    #[must_use]
    pub fn authority(&self) -> &CertificateDer<'static> {
        &self.held.authority
    }

    /// The settings a door answers with: a client certificate is required,
    /// must chain to the authority and must not be revoked, and the
    /// certificate shown is whichever this handle holds at the handshake.
    pub(crate) fn door(&self) -> Arc<ServerConfig> {
        let settings = ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(Refusing {
                inner: Arc::clone(&self.held.clients),
                keys: self.clone(),
            }))
            .with_cert_resolver(Arc::new(Showing { keys: self.clone() }));
        Arc::new(settings)
    }

    /// The settings one dial uses, presenting `mine`.
    ///
    /// `mine` is passed in rather than read here so a caller that also signs
    /// with the key — the coordinator — signs and dials with one snapshot.
    pub(crate) fn dialling(&self, mine: Credential) -> Result<ClientConfig> {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Refusing {
                inner: Arc::clone(&self.held.servers),
                keys: self.clone(),
            }))
            .with_client_auth_cert(mine.chain, mine.key)
            .map_err(transport)
    }

    fn shown(&self) -> Arc<Shown> {
        Arc::clone(
            &self
                .held
                .shown
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn revoked(&self) -> Arc<Revoked> {
        Arc::clone(
            &self
                .held
                .revoked
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Whether a handshake now would admit `presented` — asked again of a
    /// connection that outlives its handshake, so a revocation, a removal or
    /// the end of its validity reaches a held stream rather than waiting for a
    /// reconnect that a stream never makes (ADR-0108 D6, Q-901).
    ///
    /// The chain itself was verified at the handshake and does not change, so
    /// only what moves since is judged: the lists, and the clock against the
    /// leaf's `notAfter`. A date this walk cannot read leaves the stream to
    /// the lists, because the handshake that admitted it read the same bytes.
    #[must_use]
    pub(crate) fn still_admits(&self, presented: &CertificateDer<'_>) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        let lapsed = tessari_serve::tls::not_after(presented).is_some_and(|after| after < now);
        !lapsed && self.admits(presented).is_ok()
    }

    /// Refuse `presented` when its fingerprint is revoked.
    ///
    /// Asked after the authority has accepted the chain, so a certificate that
    /// is both foreign and revoked is refused for the stronger reason.
    ///
    /// A certificate naming a removed node is refused the same way: to the
    /// handshake, a node the cluster removed holds only revoked credentials.
    fn admits(&self, presented: &CertificateDer<'_>) -> std::result::Result<(), rustls::Error> {
        if self.revoked().contains(&fingerprint(presented)) {
            return Err(rustls::Error::InvalidCertificate(CertificateError::Revoked));
        }
        let removed = Arc::clone(
            &self
                .held
                .removed
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if removed
            .iter()
            .any(|node| valid_for(presented, &names(*node, Purpose::Peer)))
        {
            return Err(rustls::Error::InvalidCertificate(CertificateError::Revoked));
        }
        Ok(())
    }
}

impl Shown {
    fn of(credential: Credential) -> Result<Self> {
        let signing =
            rustls::crypto::ring::sign::any_supported_type(&credential.key).map_err(transport)?;
        let certified = CertifiedKey::new(credential.chain.clone(), signing);
        // A key that is not the leaf's makes every handshake fail at the far
        // end with a signature error nobody here would see; refused here, it
        // is refused with the reason, before anything is replaced.
        certified.keys_match().map_err(transport)?;
        Ok(Self {
            credential,
            certified: Arc::new(certified),
        })
    }
}

fn transport(why: impl std::fmt::Display) -> Error {
    Error::Transport(why.to_string())
}

/// The door's certificate, read at each handshake.
#[derive(Debug)]
struct Showing {
    keys: PeerKeys,
}

impl ResolvesServerCert for Showing {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.keys.shown().certified))
    }
}

/// A verifier that asks the authority first and the revocation list second.
#[derive(Debug)]
struct Refusing<V: ?Sized> {
    inner: Arc<V>,
    keys: PeerKeys,
}

impl ClientCertVerifier for Refusing<dyn ClientCertVerifier> {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        self.keys.admits(end_entity)?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

impl ServerCertVerifier for Refusing<WebPkiServerVerifier> {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        self.keys.admits(end_entity)?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::thread::JoinHandle;

    use tessari_encoding::NODE_ID_LEN;

    use super::{PeerKeys, Revoked};
    use crate::collection::NoLog;
    use crate::error::{Error, Result};
    use crate::grant::Deciding;
    use crate::link::tests::{Authority, hello, keys, settled};
    use crate::link::{Ask, Credential, Met, Peers, call};
    use crate::peer::Purpose;

    const DOOR: [u8; NODE_ID_LEN] = [7_u8; NODE_ID_LEN];
    const CALLER: [u8; NODE_ID_LEN] = [8_u8; NODE_ID_LEN];

    /// A door answering with `held`, greeting one caller on its own thread.
    fn serving(held: &PeerKeys) -> (std::net::SocketAddr, JoinHandle<Result<Met>>) {
        let peers = Peers::bind("127.0.0.1:0", held).expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let greeting = std::thread::spawn(move || {
            peers.greet(
                || Ok(hello(DOOR)),
                &DOOR,
                &Deciding::holding(settled()),
                &NoLog,
            )
        });
        (address, greeting)
    }

    fn dial(address: std::net::SocketAddr, caller: &PeerKeys) -> Result<()> {
        call(address, caller, DOOR, &hello(CALLER), Ask::Nothing).map(|_| ())
    }

    fn only(fingerprint: String) -> Revoked {
        Revoked::from([fingerprint])
    }

    /// The refusal a revoked certificate gets, by name — not any failure.
    ///
    /// By the text and not the variant: the door reports a failed handshake as
    /// a transport failure and a dial as the socket's, and both carry rustls's
    /// own reason.
    fn names_revocation(failure: &Error) -> bool {
        failure
            .to_string()
            .contains("invalid peer certificate: Revoked")
    }

    #[test]
    fn a_certificate_admitted_once_is_judged_again_after_a_revocation_or_a_removal() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let caller = authority.keys(CALLER, Purpose::Peer);
        let presented = caller.duplicate().chain.remove(0);
        assert!(door.still_admits(&presented));

        door.refuse(only(caller.fingerprint()));
        assert!(!door.still_admits(&presented), "a revoked certificate");

        door.refuse(Revoked::new());
        assert!(door.still_admits(&presented), "the revocation lifted");
        door.refuse_nodes(super::Removed::from([CALLER]));
        assert!(
            !door.still_admits(&presented),
            "a removed node's certificate"
        );
    }

    /// Q-901: a stream opened before its certificate's `notAfter` ends at it,
    /// as one presenting a revoked certificate does — a handshake is the only
    /// other place the date is read, and a held stream never makes another.
    #[test]
    fn a_certificate_admitted_once_is_judged_again_after_it_expires() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let current = authority.issue(CALLER, Purpose::Peer).chain.remove(0);
        let lapsed = authority.expired(CALLER, Purpose::Peer).chain.remove(0);
        assert!(door.still_admits(&current), "a certificate in its window");
        assert!(
            !door.still_admits(&lapsed),
            "a certificate past its notAfter"
        );
    }

    #[test]
    fn a_door_refuses_a_caller_whose_certificate_is_revoked() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let caller = authority.keys(CALLER, Purpose::Peer);
        door.refuse(only(caller.fingerprint()));

        let (address, greeting) = serving(&door);
        drop(dial(address, &caller));
        let refused = greeting
            .join()
            .expect("the door's thread")
            .expect_err("a revoked caller is not admitted");
        assert!(names_revocation(&refused), "{refused}");
    }

    #[test]
    fn a_caller_refuses_a_door_whose_certificate_is_revoked() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let caller = authority.keys(CALLER, Purpose::Peer);
        caller.refuse(only(door.fingerprint()));

        let (address, greeting) = serving(&door);
        let refused = dial(address, &caller).expect_err("a revoked door is not spoken to");
        assert!(names_revocation(&refused), "{refused}");
        drop(greeting.join());
    }

    #[test]
    fn a_door_refuses_a_removed_node_whatever_certificate_it_holds() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        door.refuse_nodes(super::Removed::from([CALLER]));

        // A certificate issued after the removal is still the removed node's.
        let caller = authority.keys(CALLER, Purpose::Peer);
        let (address, greeting) = serving(&door);
        drop(dial(address, &caller));
        let refused = greeting
            .join()
            .expect("the door's thread")
            .expect_err("a removed node is not admitted");
        assert!(names_revocation(&refused), "{refused}");

        // The control: another node is admitted by the same door.
        const OTHER: [u8; NODE_ID_LEN] = [6_u8; NODE_ID_LEN];
        let other = authority.keys(OTHER, Purpose::Peer);
        let (address, greeting) = serving(&door);
        call(address, &other, DOOR, &hello(OTHER), Ask::Nothing).expect("another node");
        greeting
            .join()
            .expect("the door's thread")
            .expect("another node is admitted");
    }

    #[test]
    fn a_replaced_credential_is_what_the_next_dial_presents() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let caller = authority.keys(CALLER, Purpose::Peer);
        // The door refuses the caller's first certificate, so only the new one
        // can be what gets the second dial in.
        door.refuse(only(caller.fingerprint()));
        let (address, greeting) = serving(&door);
        drop(dial(address, &caller));
        drop(greeting.join());

        caller
            .replace(authority.issue(CALLER, Purpose::Peer))
            .expect("a credential the authority issued");
        let (address, greeting) = serving(&door);
        dial(address, &caller).expect("the new certificate is presented and admitted");
        let met = greeting
            .join()
            .expect("the door's thread")
            .expect("the door admits the new certificate");
        assert_eq!(met.said.node, CALLER);
    }

    #[test]
    fn a_door_presents_its_replaced_credential_from_the_next_handshake() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let caller = authority.keys(CALLER, Purpose::Peer);
        caller.refuse(only(door.fingerprint()));
        // One door, held across the replacement: the bind is not redone, so
        // what changes is what the door resolves at the handshake.
        let peers = Peers::bind("127.0.0.1:0", &door).expect("a peer door on loopback");
        let address = peers.address().expect("the door's address");
        let answering = std::thread::spawn(move || {
            let deciding = Deciding::holding(settled());
            let first = peers.greet(|| Ok(hello(DOOR)), &DOOR, &deciding, &NoLog);
            let second = peers.greet(|| Ok(hello(DOOR)), &DOOR, &deciding, &NoLog);
            (first, second)
        });

        let refused = dial(address, &caller).expect_err("the old certificate is refused");
        assert!(names_revocation(&refused), "{refused}");
        door.replace(authority.issue(DOOR, Purpose::Peer))
            .expect("a credential the authority issued");
        dial(address, &caller).expect("the door now presents its new certificate");
        let (_, second) = answering.join().expect("the door's thread");
        assert_eq!(
            second.expect("the second caller is admitted").said.node,
            CALLER
        );
    }

    #[test]
    fn a_replacement_whose_key_is_not_the_certificates_is_refused_and_changes_nothing() {
        let authority = Authority::new();
        let held = authority.keys(CALLER, Purpose::Peer);
        let before = held.fingerprint();
        let other = authority.issue(CALLER, Purpose::Peer);
        let mismatched = Credential {
            chain: authority.issue(CALLER, Purpose::Peer).chain,
            key: other.key,
        };

        let refused = held
            .replace(mismatched)
            .expect_err("a key that is not the leaf's");
        assert!(
            matches!(&refused, Error::Transport(why) if why.contains("KeyMismatch")),
            "{refused}"
        );
        assert_eq!(held.fingerprint(), before, "the old credential is kept");

        let door = authority.keys(DOOR, Purpose::Peer);
        let (address, greeting) = serving(&door);
        dial(address, &held).expect("the kept credential still works");
        drop(greeting.join());
    }

    #[test]
    fn an_expired_certificate_is_refused_at_the_handshake() {
        let authority = Authority::new();
        let door = authority.keys(DOOR, Purpose::Peer);
        let expired = keys(authority.expired(CALLER, Purpose::Peer), &authority.der())
            .expect("an expired credential is still a matching pair");

        let (address, greeting) = serving(&door);
        drop(dial(address, &expired));
        let refused = greeting
            .join()
            .expect("the door's thread")
            .expect_err("an expired certificate is not admitted");
        assert!(
            matches!(&refused, Error::Transport(why) if why.to_lowercase().contains("expired")),
            "{refused}"
        );
    }
}
