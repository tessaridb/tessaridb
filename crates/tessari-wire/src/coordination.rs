//! A request carried over the peer link to the node that can answer it
//! (ADR-0108 D1–D3).
//!
//! # The two halves
//!
//! [`Coordinator`] is the asking half, installed on the store by a process that
//! knows its peers: it signs an [`Assertion`] that it acts for the caller it
//! verified, and sends it with the request. [`admit_asserted`] is the answering half's
//! judgement, after the door has checked the signature: the user the assertion
//! names must be the same account in THIS node's catalog, and inside the reach
//! the asserting peer is subscribed to. The answering node then runs the script
//! as that user, and its own grants decide — the asking node never widens
//! anybody.
//!
//! # What never crosses
//!
//! A password. The old forward relayed the caller's name and password to
//! another node's plaintext client surface (R-10); this carries an id, a digest
//! of the account and a signature, over the mutually authenticated link.
//!
//! # One hop
//!
//! A coordinated request is answered by the node it reaches or refused there.
//! Nothing on the answering side coordinates again, so a stale view cannot send
//! a request round the cluster (Q-109).

use std::sync::Weak;
use std::time::Duration;

use sha2::{Digest, Sha256};

use tessari_constants::COORDINATED_SECONDS;
use tessari_encoding::{NODE_ID_LEN, decode_payload, encode_payload};
use tessari_storage::{Catalog, UserDefinition};
use tessari_types::Reach;
use tessaridb::{Coordinate as Coordinates, Coordinated, Coordination, Db, Parameters, Surface};

use crate::assertion::{Assertion, Principal, Signed, nonce, now_ms, request_digest};
use crate::error::{Error, Result};
use crate::frame;
use crate::gatherer::Greeting;
use crate::keys::PeerKeys;
use crate::link::{Answered, Ask, call_within};

/// How long an assertion this node makes is good for.
pub(crate) const LIFE_MILLIS: u64 = 10_000;

/// A request, the assertion it travels under, and where its caller waits.
#[derive(Debug, Clone, PartialEq)]
pub struct Coordinate {
    /// Who it is asked for, signed by the asking node.
    pub signed: Signed,
    /// The namespace the caller's session had selected.
    pub namespace: Option<String>,
    /// The database the caller's session had selected.
    pub database: Option<String>,
    /// The script as the caller sent it.
    pub script: String,
    /// Its bound values.
    pub parameters: Parameters,
    /// The shape the answer must take.
    pub surface: Surface,
}

impl Coordinate {
    /// What the assertion must have been made for.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        request_digest(
            self.namespace.as_deref(),
            self.database.as_deref(),
            &self.script,
            &encode_parameters(&self.parameters),
        )
    }

    /// The body of a [`crate::peer::PeerFrame::Coordinate`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = self.signed.encode();
        for selected in [&self.namespace, &self.database] {
            match selected {
                Some(name) => {
                    body.push(1);
                    frame::put_text(&mut body, name);
                }
                None => body.push(0),
            }
        }
        frame::put_text(&mut body, &self.script);
        frame::put_bytes(&mut body, &encode_parameters(&self.parameters));
        match self.surface {
            Surface::Wire { minor } => body.extend_from_slice(&[0, minor]),
            Surface::Http => body.push(1),
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] when the body does not hold a whole request.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (signed, mut at) = Signed::decode(body, 0)?;
        let mut selected = [None, None];
        for slot in &mut selected {
            let flag = body.get(at).copied().ok_or(Error::Malformed)?;
            at = at.checked_add(1).ok_or(Error::Malformed)?;
            if flag == 1 {
                let (name, next) = frame::take_text(body, at)?;
                *slot = Some(name);
                at = next;
            } else if flag != 0 {
                return Err(Error::Malformed);
            }
        }
        let [namespace, database] = selected;
        let (script, at) = frame::take_text(body, at)?;
        let (parameters, at) = frame::take_bytes(body, at)?;
        let surface = match body.get(at..) {
            Some([0, minor]) => Surface::Wire { minor: *minor },
            Some([1]) => Surface::Http,
            _ => return Err(Error::Malformed),
        };
        Ok(Self {
            signed,
            namespace,
            database,
            script,
            parameters: decode_parameters(&parameters)?,
            surface,
        })
    }
}

/// The body of a [`crate::peer::PeerFrame::Coordinated`] frame.
#[must_use]
pub fn encode_answer(answer: &Coordinated) -> Vec<u8> {
    let mut body = Vec::with_capacity(answer.body.len().saturating_add(2));
    body.extend_from_slice(&answer.kind.to_be_bytes());
    body.extend_from_slice(&answer.body);
    body
}

/// Read one back.
///
/// # Errors
///
/// [`Error::Malformed`] for a body too short to carry a kind.
pub fn decode_answer(body: &[u8]) -> Result<Coordinated> {
    let (kind, rest) = body.split_first_chunk::<2>().ok_or(Error::Malformed)?;
    Ok(Coordinated {
        kind: u16::from_be_bytes(*kind),
        body: rest.to_vec(),
    })
}

/// Bound values in the store's own codec, in key order — so two nodes digest
/// one set of values to one digest.
pub(crate) fn encode_parameters(parameters: &Parameters) -> Vec<u8> {
    let mut body = Vec::new();
    frame::put_u32(
        &mut body,
        u32::try_from(parameters.len()).unwrap_or(u32::MAX),
    );
    for (name, value) in parameters {
        frame::put_text(&mut body, name);
        frame::put_bytes(&mut body, encode_payload(value).as_slice());
    }
    body
}

/// Read them back.
pub(crate) fn decode_parameters(body: &[u8]) -> Result<Parameters> {
    let (count, mut at) = frame::take_u32(body, 0)?;
    let mut parameters = Parameters::new();
    for _ in 0..count {
        let (name, next) = frame::take_text(body, at)?;
        let (bytes, next) = frame::take_bytes(body, next)?;
        parameters.insert(name, decode_payload(&bytes)?);
        at = next;
    }
    Ok(parameters)
}

/// The digest of an account as its catalog row stands — what a node that
/// verified the user's password asserts, and what the answering node compares
/// with its own row. A rotated password, a changed role or a dropped and
/// re-declared user each change it.
#[must_use]
pub fn account(user: &UserDefinition) -> [u8; 32] {
    Sha256::digest(encode_payload(&user.to_value()).as_slice()).into()
}

/// The answering half's judgement of a believed assertion: the session to run
/// the request in, acting as the user it names.
///
/// # Errors
///
/// In words the asking node passes to its caller: the account changed or is
/// gone, the user reaches past what the asking peer is subscribed to, or the
/// catalog could not be read.
pub fn admit_asserted<'a>(
    db: &'a Db,
    from: [u8; NODE_ID_LEN],
    assertion: &Assertion,
) -> std::result::Result<tessaridb::Session<'a>, String> {
    let mut session = db.session();
    let Principal::User { id, account: held } = assertion.principal else {
        // Nobody signed in: the session's own rule answers, which on a store
        // with a user refuses whatever the script does.
        return Ok(session);
    };
    let mut transaction = db.store().begin().map_err(|why| why.to_string())?;
    let catalog = Catalog::new(&mut transaction);
    let users = catalog.users().map_err(|why| why.to_string())?;
    let replicas = catalog.replicas().map_err(|why| why.to_string())?;
    transaction.rollback();
    let user = users
        .iter()
        .find(|user| user.id == id)
        .filter(|user| account(user) == held)
        .ok_or("the account that node verified has changed here; sign in again")?;
    let subscribed = replicas
        .iter()
        .find(|row| row.node == Some(from))
        .and_then(|row| row.replicates)
        .ok_or("the asking node is subscribed to nothing here, so it may act for nobody")?;
    let reach = match (user.namespace, user.database) {
        (None, _) => Reach::Store,
        (Some(namespace), None) => Reach::Namespace(namespace),
        (Some(namespace), Some(database)) => Reach::Database(namespace, database),
    };
    if !subscribed.contains(reach) {
        return Err("that user reaches past what the asking node is subscribed to here".to_owned());
    }
    session.acting_as(id).map_err(|why| why.to_string())?;
    Ok(session)
}

/// The asking half: carries a request to the node that can answer it.
pub struct Coordinator {
    db: Weak<Db>,
    me: [u8; NODE_ID_LEN],
    keys: PeerKeys,
    greeting: Greeting,
    /// Links kept open for the next record of a transaction across leaders
    /// (ADR-0112 D13j).
    kept: crate::across::kept::KeptLinks,
}

impl core::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Coordinator")
            .field("me", &self.me)
            .finish_non_exhaustive()
    }
}

impl Coordinator {
    /// A coordinator for the node that holds `db`, speaking as `me` with its
    /// peer `credential`, whose key also signs every assertion.
    ///
    /// `db` is a weak handle because the store holds the coordinator: a strong
    /// one would keep a stopped node's store alive for as long as it does.
    #[must_use]
    pub fn new(db: Weak<Db>, me: [u8; NODE_ID_LEN], keys: PeerKeys, greeting: Greeting) -> Self {
        Self {
            db,
            me,
            keys,
            greeting,
            kept: crate::across::kept::KeptLinks::default(),
        }
    }
}

impl Coordinator {
    /// This node's id, which every assertion it makes is from.
    pub(crate) const fn me(&self) -> [u8; NODE_ID_LEN] {
        self.me
    }

    /// The credentials this node dials with.
    pub(crate) const fn keys(&self) -> &PeerKeys {
        &self.keys
    }

    /// The links kept for the next record of a transaction across leaders.
    pub(crate) const fn kept(&self) -> &crate::across::kept::KeptLinks {
        &self.kept
    }

    /// Where `to` answers, the one credential snapshot that both signs and
    /// dials, and this node's greeting — everything a carried request needs
    /// before it is built.
    ///
    /// One snapshot signs and dials: the far end checks the assertion against
    /// the certificate this connection presents, so a rotation landing between
    /// the two would refuse a request that was honest.
    pub(crate) fn dialling(
        &self,
        to: [u8; NODE_ID_LEN],
    ) -> std::result::Result<(String, crate::link::Credential, crate::peer::Hello), String> {
        if to == self.me {
            return Err("this node is the one that answers; it does not ask itself".to_owned());
        }
        let db = self.db.upgrade().ok_or("this node is stopping")?;
        let endpoint = db
            .member(&to)
            .map_err(|why| why.to_string())?
            .map(|row| row.endpoint)
            .ok_or("the node that answers is not declared here")?;
        let mine = self.keys.duplicate();
        let said = (self.greeting)().map_err(|why| why.to_string())?;
        Ok((endpoint, mine, said))
    }
}

impl Coordinates for Coordinator {
    fn coordinate(&self, asked: &Coordination<'_>) -> std::result::Result<Coordinated, String> {
        let (endpoint, mine, said) = self.dialling(asked.to)?;
        let request = Coordinate {
            signed: Assertion {
                from: self.me,
                to: asked.to,
                principal: asked
                    .user
                    .map_or(Principal::Anonymous, |user| Principal::User {
                        id: user.id,
                        account: account(user),
                    }),
                request: request_digest(
                    asked.namespace,
                    asked.database,
                    asked.script,
                    &encode_parameters(asked.parameters),
                ),
                nonce: nonce().map_err(|why| why.to_string())?,
                issued_ms: now_ms(),
                expires_ms: now_ms().saturating_add(LIFE_MILLIS),
            }
            .sign(&mine.key)
            .map_err(|why| why.to_string())?,
            namespace: asked.namespace.map(str::to_owned),
            database: asked.database.map(str::to_owned),
            script: asked.script.to_owned(),
            parameters: asked.parameters.clone(),
            surface: asked.surface,
        };
        match call_within(
            endpoint.as_str(),
            (&self.keys, mine),
            asked.to,
            &said,
            Ask::Coordinate(&request),
            Duration::from_secs(COORDINATED_SECONDS),
        ) {
            Ok((_, Answered::Coordinated(answer))) => Ok(answer),
            Ok(_) => Err(format!(
                "{endpoint} answered something other than the request"
            )),
            Err(Error::NotCoordinated(why)) => Err(format!("{endpoint} refused: {why}")),
            Err(why) => Err(format!("{endpoint}: {why}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustls::pki_types::PrivateKeyDer;
    use tessaridb::{Coordinate as _, Coordination, Db, Parameters, Surface};

    use super::Coordinator;
    use crate::link::Credential;

    const ME: [u8; tessari_encoding::NODE_ID_LEN] = [1; tessari_encoding::NODE_ID_LEN];

    fn coordinator(db: &Arc<Db>) -> Coordinator {
        let key = rcgen::KeyPair::generate().expect("a key");
        let certificate = rcgen::CertificateParams::new(vec!["me.peer.tessari".to_owned()])
            .expect("params")
            .self_signed(&key)
            .expect("a certificate");
        Coordinator::new(
            Arc::downgrade(db),
            ME,
            crate::keys::PeerKeys::new(
                Credential {
                    chain: vec![certificate.der().clone()],
                    key: PrivateKeyDer::try_from(key.serialize_der()).expect("a key in DER"),
                },
                certificate.der().clone(),
            )
            .expect("a usable credential"),
            Box::new(|| Err(crate::gatherer::GreetingUnavailable::Stopping)),
        )
    }

    #[test]
    fn a_request_is_never_carried_to_the_node_that_holds_it() {
        // R-14 / Q-109: a drained leader whose catalog still names it writable
        // used to forward to itself until it ran out of connections. The hop
        // is now named by node id, and a node never asks itself.
        let db = Arc::new(Db::in_memory().expect("a store"));
        let refused = coordinator(&db)
            .coordinate(&Coordination {
                to: ME,
                user: None,
                namespace: None,
                database: None,
                script: "CREATE t:1;",
                parameters: &Parameters::new(),
                surface: Surface::Http,
            })
            .expect_err("a node carried a request to itself");
        assert!(refused.contains("does not ask itself"), "{refused}");
    }
}
