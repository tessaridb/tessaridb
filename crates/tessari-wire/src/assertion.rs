//! A node's signed statement that it acts for a user (ADR-0108 D3).
//!
//! # Why a signature when the link is already mutual TLS
//!
//! The TLS session proves which node is on the other end and protects every
//! byte for that one hop. What it does not leave behind is a record: once the
//! connection closes nothing shows which node asked a leader to act as which
//! user. The assertion is that record — the answering node keeps it in its audit
//! trail, signed by the node that made it, so a misbehaving member is
//! attributable after the fact. And it is bound to the request it was made for,
//! to the node it was made to, to a short life and to a nonce, so a captured
//! assertion cannot be replayed, redirected or reused for another script.
//!
//! # Signed with the key the link already proves
//!
//! No second key exists to lose or rotate: the assertion is signed with the
//! node's peer key, and checked against the certificate the SAME connection's
//! handshake proved. A frame cannot therefore claim another signer — the
//! asserting node must be the peer the handshake named, and its signature must
//! verify under that peer's certificate.
//!
//! # What it never carries
//!
//! A password. The user is named by id and by a digest of their catalog row as
//! the asserting node verified it, so a rotated password, a changed role or a
//! dropped account no longer matches on the answering node — the rule that ends
//! a token, applied to a hop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rustls::SignatureScheme;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

use tessari_encoding::NODE_ID_LEN;

use crate::error::{Error, Result};
use crate::frame;

/// Every message this module signs starts with this, so a signature over an
/// assertion can never be presented as a signature over anything else the
/// node's key signs — a TLS handshake included.
const DOMAIN: &[u8] = b"tessaridb principal assertion v1\0";

/// The longest life an assertion may claim.
pub const LONGEST_LIFE_MILLIS: u64 = 30_000;

/// How far apart two nodes' clocks may be before an assertion is refused as
/// not yet valid.
pub const CLOCK_SKEW_MILLIS: u64 = 5_000;

/// How many live nonces the replay table holds before it refuses rather than
/// forgets one — forgetting would let the forgotten assertion be replayed.
pub const REPLAY_TABLE: usize = 65_536;

/// The schemes an assertion may be signed with: those TLS 1.3 allows, so any
/// key a peer credential can hold.
const SCHEMES: [SignatureScheme; 6] = [
    SignatureScheme::ED25519,
    SignatureScheme::ECDSA_NISTP256_SHA256,
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA512,
];

/// Who the asserting node acts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    /// Nobody signed in — answerable only on a store with no user.
    Anonymous,
    /// A declared user.
    User {
        /// The user's catalog id.
        id: u32,
        /// SHA-256 of the user's catalog row as the asserting node verified it.
        account: [u8; 32],
    },
}

/// The statement itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assertion {
    /// The node making it.
    pub from: [u8; NODE_ID_LEN],
    /// The node it is made to.
    pub to: [u8; NODE_ID_LEN],
    /// Who it acts for.
    pub principal: Principal,
    /// What it is for: [`request_digest`] of the request it travels with.
    pub request: [u8; 32],
    /// Never twice.
    pub nonce: [u8; 16],
    /// When it was made, in unix milliseconds.
    pub issued_ms: u64,
    /// When it stops being good, in unix milliseconds.
    pub expires_ms: u64,
}

/// An assertion and the signature over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// What was asserted.
    pub assertion: Assertion,
    scheme: u16,
    signature: Vec<u8>,
}

/// Why an assertion was not believed. Each says what to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Disbelieved {
    /// It names a signer other than the peer the handshake proved.
    #[error("the assertion names a node other than the one on this connection")]
    NotTheSigner,
    /// It was made to another node.
    #[error("the assertion was made to another node")]
    NotForThisNode,
    /// Signed with a scheme no peer credential may use.
    #[error("the assertion is signed with a scheme this node does not accept")]
    UnsupportedScheme,
    /// The signature does not verify under the connection's certificate.
    #[error("the assertion's signature does not verify")]
    BadSignature,
    /// Its life is over.
    #[error("the assertion has expired")]
    Expired,
    /// It claims to have been made in the future, past the clock allowance.
    #[error("the assertion is dated in the future")]
    NotYetValid,
    /// It claims a life longer than any assertion may have.
    #[error("the assertion claims a life longer than {LONGEST_LIFE_MILLIS} ms")]
    TooLong,
    /// It was made for a different request.
    #[error("the assertion was made for a different request")]
    RequestAltered,
    /// Its nonce was seen before.
    #[error("the assertion was already used")]
    Replayed,
    /// Too many live assertions to remember another; refused, never forgotten.
    #[error("too many assertions are live to remember another; try again shortly")]
    ReplayTableFull,
}

/// The nonces seen and still inside their assertion's life.
#[derive(Debug, Default)]
pub struct Replays {
    seen: DashMap<[u8; 16], u64>,
    held: AtomicUsize,
}

impl Replays {
    /// Remember `nonce` until `expires_ms`, or say it was seen.
    fn remember(
        &self,
        nonce: [u8; 16],
        expires_ms: u64,
        now_ms: u64,
    ) -> std::result::Result<(), Disbelieved> {
        if self.held.load(Ordering::Relaxed) >= REPLAY_TABLE {
            self.seen.retain(|_, expires| *expires >= now_ms);
            self.held.store(self.seen.len(), Ordering::Relaxed);
            if self.held.load(Ordering::Relaxed) >= REPLAY_TABLE {
                return Err(Disbelieved::ReplayTableFull);
            }
        }
        match self.seen.entry(nonce) {
            dashmap::Entry::Occupied(_) => Err(Disbelieved::Replayed),
            dashmap::Entry::Vacant(vacant) => {
                vacant.insert(expires_ms);
                self.held.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

/// The digest that binds an assertion to the request it travels with.
#[must_use]
pub fn request_digest(
    namespace: Option<&str>,
    database: Option<&str>,
    script: &str,
    parameters: &[u8],
) -> [u8; 32] {
    let mut hashed = Vec::new();
    for selected in [namespace, database] {
        match selected {
            Some(name) => {
                hashed.push(1);
                frame::put_text(&mut hashed, name);
            }
            None => hashed.push(0),
        }
    }
    frame::put_text(&mut hashed, script);
    frame::put_bytes(&mut hashed, parameters);
    Sha256::digest(&hashed).into()
}

/// The wall clock in unix milliseconds; zero for a clock before 1970.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// A fresh nonce from the operating system.
///
/// # Errors
///
/// [`Error::Transport`] when the system's randomness cannot be read; an
/// assertion is then not made rather than made with a guessable nonce.
pub fn nonce() -> Result<[u8; 16]> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| Error::Transport("the system's randomness could not be read".to_owned()))?;
    Ok(nonce)
}

impl Assertion {
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(128);
        body.extend_from_slice(&self.from);
        body.extend_from_slice(&self.to);
        match self.principal {
            Principal::Anonymous => body.push(0),
            Principal::User { id, account } => {
                body.push(1);
                frame::put_u32(&mut body, id);
                body.extend_from_slice(&account);
            }
        }
        body.extend_from_slice(&self.request);
        body.extend_from_slice(&self.nonce);
        frame::put_u64(&mut body, self.issued_ms);
        frame::put_u64(&mut body, self.expires_ms);
        body
    }

    fn decode(from: &[u8], at: usize) -> Result<(Self, usize)> {
        let (from_node, at) = take_array::<NODE_ID_LEN>(from, at)?;
        let (to, at) = take_array::<NODE_ID_LEN>(from, at)?;
        let (kind, at) = take_array::<1>(from, at)?;
        let (principal, at) = match kind {
            [0] => (Principal::Anonymous, at),
            [1] => {
                let (id, at) = frame::take_u32(from, at)?;
                let (account, at) = take_array::<32>(from, at)?;
                (Principal::User { id, account }, at)
            }
            _ => return Err(Error::Malformed),
        };
        let (request, at) = take_array::<32>(from, at)?;
        let (nonce, at) = take_array::<16>(from, at)?;
        let (issued_ms, at) = frame::take_u64(from, at)?;
        let (expires_ms, at) = frame::take_u64(from, at)?;
        Ok((
            Self {
                from: from_node,
                to,
                principal,
                request,
                nonce,
                issued_ms,
                expires_ms,
            },
            at,
        ))
    }

    /// What is signed: the domain, then the assertion.
    fn message(&self) -> Vec<u8> {
        let mut message = DOMAIN.to_vec();
        message.extend_from_slice(&self.encode());
        message
    }

    /// Sign this assertion with `key`, the node's peer key.
    ///
    /// # Errors
    ///
    /// [`Error::Transport`] when the key is of a kind no accepted scheme signs
    /// with, or signing fails.
    pub fn sign(self, key: &PrivateKeyDer<'_>) -> Result<Signed> {
        let signing = rustls::crypto::ring::sign::any_supported_type(key)
            .map_err(|why| Error::Transport(why.to_string()))?;
        let signer = signing.choose_scheme(&SCHEMES).ok_or_else(|| {
            Error::Transport("the node's key signs with no scheme an assertion accepts".to_owned())
        })?;
        let signature = signer
            .sign(&self.message())
            .map_err(|why| Error::Transport(why.to_string()))?;
        Ok(Signed {
            assertion: self,
            scheme: u16::from(signer.scheme()),
            signature,
        })
    }
}

impl Signed {
    /// The bytes a frame carries.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = self.assertion.encode();
        body.extend_from_slice(&self.scheme.to_be_bytes());
        frame::put_bytes(&mut body, &self.signature);
        body
    }

    /// Read one back from `from` at `at`, and where it ended.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] for anything that is not a whole signed assertion.
    pub fn decode(from: &[u8], at: usize) -> Result<(Self, usize)> {
        let (assertion, at) = Assertion::decode(from, at)?;
        let (scheme, at) = take_array::<2>(from, at)?;
        let (signature, at) = frame::take_bytes(from, at)?;
        Ok((
            Self {
                assertion,
                scheme: u16::from_be_bytes(scheme),
                signature,
            },
            at,
        ))
    }

    /// Believe this assertion, or say why not.
    ///
    /// `shown` is the certificate this connection's handshake proved and
    /// `proven` the node it names; `me` is this node; `request` is
    /// [`request_digest`] of the request it arrived with. Checked in an order
    /// where nothing cheap is skipped and nothing is remembered for an
    /// assertion that fails: the replay table records a nonce only once
    /// everything else held.
    ///
    /// # Errors
    ///
    /// The [`Disbelieved`] reason.
    pub fn verify(
        &self,
        shown: &CertificateDer<'_>,
        proven: [u8; NODE_ID_LEN],
        me: [u8; NODE_ID_LEN],
        (request, now_ms): ([u8; 32], u64),
        replays: &Replays,
    ) -> std::result::Result<&Assertion, Disbelieved> {
        let assertion = &self.assertion;
        if assertion.from != proven {
            return Err(Disbelieved::NotTheSigner);
        }
        if assertion.to != me {
            return Err(Disbelieved::NotForThisNode);
        }
        let scheme = SignatureScheme::from(self.scheme);
        if !SCHEMES.contains(&scheme) {
            return Err(Disbelieved::UnsupportedScheme);
        }
        let algorithms = rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .mapping
            .iter()
            .find(|(named, _)| *named == scheme)
            .map(|(_, algorithms)| *algorithms)
            .ok_or(Disbelieved::UnsupportedScheme)?;
        let certificate =
            webpki::EndEntityCert::try_from(shown).map_err(|_| Disbelieved::BadSignature)?;
        let message = assertion.message();
        if !algorithms.iter().any(|algorithm| {
            certificate
                .verify_signature(*algorithm, &message, &self.signature)
                .is_ok()
        }) {
            return Err(Disbelieved::BadSignature);
        }
        let life = assertion
            .expires_ms
            .checked_sub(assertion.issued_ms)
            .ok_or(Disbelieved::TooLong)?;
        if life > LONGEST_LIFE_MILLIS {
            return Err(Disbelieved::TooLong);
        }
        if now_ms > assertion.expires_ms {
            return Err(Disbelieved::Expired);
        }
        if assertion.issued_ms > now_ms.saturating_add(CLOCK_SKEW_MILLIS) {
            return Err(Disbelieved::NotYetValid);
        }
        if assertion.request != request {
            return Err(Disbelieved::RequestAltered);
        }
        replays.remember(assertion.nonce, assertion.expires_ms, now_ms)?;
        Ok(assertion)
    }
}

/// A fixed number of bytes, and where they ended.
fn take_array<const N: usize>(from: &[u8], at: usize) -> Result<([u8; N], usize)> {
    let end = at.checked_add(N).ok_or(Error::Malformed)?;
    let bytes = from.get(at..end).ok_or(Error::Malformed)?;
    let mut held = [0_u8; N];
    held.copy_from_slice(bytes);
    Ok((held, end))
}

#[cfg(test)]
mod tests;
