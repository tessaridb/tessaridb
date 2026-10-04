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

mod ballots;
mod greeting;
mod majority;
mod ranges;

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
        let mut params =
            rcgen::CertificateParams::new(vec![names(node, purpose)]).expect("a leaf's parameters");
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
    let answering = std::thread::spawn(move || peers.greet(|| Ok(mine), &HERE, &deciding, &NoLog));
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
