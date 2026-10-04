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
mod tests;
