//! What a node was told about the cluster it belongs to.
//!
//! # Configuration, not data, and the concept settled it
//!
//! A node's identity and roles come from its own store — `NodeIdentity` is
//! generated on first open and stable across restarts. Its declared membership
//! comes from the catalog, where `ReplicaDefinition` already keeps it. What
//! arrives here is the rest: the **peer credential**, the **cluster
//! authority**, the **address this node's own door binds** and the **seed
//! addresses** to dial for a first contact.
//!
//! Those three are configuration because the alternative fails twice. It fails
//! on bootstrap order — the credential needed to *reach* a peer would be read
//! from the store that peer is supposed to help populate — and it fails on
//! restore, because a node restored from a backup would inherit the cluster
//! membership of whoever it was copied from. The code had already taken this
//! position elsewhere: `NodeIdentity` carries `endpoints` and documents them as
//! local, *"a fact about this machine's network position … a node that restores
//! a backup must not inherit the original's address."* A private key is the same
//! class of fact as an address, only more so.
//!
//! # Half a cluster is not a configuration
//!
//! A node told some of the parts and not the rest does not start. This is the
//! posture the first-user bootstrap already takes, for its stated reason: a node
//! that came up **open** because its credentials were misconfigured should never
//! have reached the point of answering on a network. A half-configured cluster
//! node is that same failure wearing different clothes — it would answer while
//! holding peers it cannot reach, or cannot prove itself to, and it would look
//! exactly like a node that started correctly.
//!
//! Told **none** of them is a different thing entirely, and it is not an error.
//! Every deployment of this engine today is a single node, so absent is the
//! common case and a supported one. It is not warned about either: a log line
//! printed on every start of every unclustered node is a log line operators stop
//! reading, and it would be the one that matters when it finally changes.
//!
//! # Paths in, and no search
//!
//! Nothing here looks for a credential. There is no default location, no search
//! order and no environment fallback chain, because a search order does not fail
//! when the operator's file is missing — it finds a *different* one, and a node
//! holding the wrong credential is a node joining a cluster nobody meant it to
//! join. Reading a path that was named can only ever fail loudly.

use std::path::{Path, PathBuf};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tessari_encoding::NODE_ID_LEN;

use crate::error::{Error, Result};
use crate::link::Credential;

/// The peer credential's chain, named in refusals.
const CHAIN: &str = "this node's peer credential";
/// The peer credential's private key, named in refusals.
const KEY: &str = "this node's private key";
/// The cluster's authority certificate, named in refusals.
const AUTHORITY: &str = "the cluster authority";

/// Where the three parts of a cluster configuration were said to be.
///
/// Holds paths and nothing read from them, so the all-or-nothing rule is settled
/// before any file is opened — an operator who forgot a flag learns it from the
/// flags rather than from whichever file happened to be missing as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Told {
    /// The PEM file holding this node's certificate and any intermediates.
    pub chain: PathBuf,
    /// The PEM file holding the private key for that certificate's leaf.
    pub key: PathBuf,
    /// The PEM file holding the one certificate every peer is issued by.
    pub authority: PathBuf,
    /// Where this node's own peer door binds.
    ///
    /// A part like the other four, with no default, because there is no port
    /// this engine claims — see `link.rs`, which takes an address for the same
    /// reason. It is also the one part of a cluster configuration whose default
    /// would have a security consequence: defaulted wide it is a door the
    /// operator did not know they opened, and defaulted to loopback it is a
    /// cluster that cannot form. An operator told to name it names it.
    pub door: String,
    /// Addresses to dial for a first contact.
    ///
    /// May be empty, and an empty list is not a half-configuration. A node that
    /// IS the cluster has nowhere to be reached the cluster *through*, and a
    /// node whose catalog already names a peer has somewhere better to look —
    /// see [`Self::from_parts`] for why the check that used to live here could
    /// not answer either case.
    pub seeds: Vec<String>,
}

impl Told {
    /// What the cluster flags amount to: all of it, none of it, or a refusal.
    ///
    /// **Four parts are all-or-nothing** — the credential, its key, the
    /// authority that issued both, and the address this node's own door binds.
    /// Each is about proving an identity or being reachable at all, and any
    /// subset of them describes a node that would come up believing it has
    /// peers it cannot prove itself to.
    ///
    /// # Why the seeds are not the fifth
    ///
    /// They were, until this wave, and the constraint became visible the moment
    /// the flag started being read (Q-577): a founding node — the first one,
    /// whose catalog already names its peers — has nowhere to be reached the
    /// cluster *through*, and was made to name an address anyway so that a list
    /// could be non-empty. The value was inert on every such node.
    ///
    /// The check it was standing in for is real and it is not this one. *Can
    /// this node reach anybody* is answered by the seeds **or** by the catalog,
    /// and this function reads flags and not a store, so it can see one half of
    /// a disjunction and never the other. Asked here it refuses the founding
    /// node; asked where the store is open it refuses exactly the node that has
    /// no route to anyone. It is asked there instead.
    ///
    /// An empty list given alongside none of the four is still not a cluster:
    /// that is the unclustered node, and it answers `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ClusterHalfConfigured`] when some parts were given and
    /// others were not, naming both sides.
    pub fn from_parts(
        chain: Option<PathBuf>,
        key: Option<PathBuf>,
        authority: Option<PathBuf>,
        door: Option<String>,
        seeds: Vec<String>,
    ) -> Result<Option<Self>> {
        let present = [
            ("a peer credential", chain.is_some()),
            ("a private key", key.is_some()),
            ("a cluster authority", authority.is_some()),
            ("a peer address", door.is_some()),
        ];
        let mut given: Vec<&str> = present.iter().filter(|p| p.1).map(|p| p.0).collect();
        let missing: Vec<&str> = present.iter().filter(|p| !p.1).map(|p| p.0).collect();
        if !seeds.is_empty() {
            // Named among what was given but never among what is missing: a
            // seed cannot complete a configuration and its absence cannot
            // break one, yet an operator who passed only `--seed` has to be
            // told their four other flags never arrived.
            given.push("seed addresses");
        }
        if given.is_empty() {
            return Ok(None);
        }
        // Destructured together rather than unwrapped one at a time: all of them
        // are present exactly when nothing is missing, and asking each `Option`
        // again would be a second statement of a fact this list already carries.
        let (Some(chain), Some(key), Some(authority), Some(door)) = (chain, key, authority, door)
        else {
            return Err(Error::ClusterHalfConfigured {
                given: given.join(", "),
                missing: missing.join(", "),
            });
        };
        Ok(Some(Self {
            chain,
            key,
            authority,
            door,
            seeds,
        }))
    }
}

/// One address to reach an existing cluster through, and who answers there.
///
/// # Why a seed names a node
///
/// The obvious spelling is a bare `host:port`, and it is the one `--seed` had
/// until ADR-0067. It cannot be dialled. [`crate::call`] derives the TLS server
/// name from the peer's id — `<id>.peer.tessari` — so the handshake refuses any
/// node but the one the caller meant, which is ADR-0062's whole point: *the
/// caller names the node it means to reach*. An address with no id attached is
/// therefore not a dial this transport can express, and wiring one through
/// would have failed at the handshake and read like a certificate problem.
///
/// So the operator writes down the id, which is the value `INFO FOR NODE`
/// already prints. The alternative — accepting any credential this cluster's
/// authority issued, whoever answers at that address — is a real option and is
/// deliberately not taken here: it replaces an exact name with *any member* on
/// the security-critical path, and that is the concept's own open question C-17
/// rather than something to settle as a side effect of a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    /// The node expected to answer there, checked by the handshake.
    pub node: [u8; NODE_ID_LEN],
    /// Where it answers.
    ///
    /// Carried unparsed for the reason [`Joining::door`] is: whether an address
    /// resolves is a question for whoever dials it, and refusing it here would
    /// make startup depend on the network being up at that instant.
    pub endpoint: String,
}

impl Seed {
    /// Read one `--seed` value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SeedMalformed`] when the value is not
    /// `<node-id>@<host:port>`, naming which half was wrong.
    pub fn parse(given: &str) -> Result<Self> {
        // `rsplit_once`, not `split_once`: the id half holds no `@` and the
        // address half is the remainder, so splitting at the LAST separator
        // would silently accept an id containing one. Splitting at the first
        // and refusing an unparseable id is the stricter of the two, and the
        // refusal names the half rather than the whole.
        let (id, endpoint) = given.split_once('@').ok_or_else(|| Error::SeedMalformed {
            given: given.to_owned(),
            reason: "there is no @ between the node id and the address",
        })?;
        let node = tessari_types::parse_uuid(id).ok_or_else(|| Error::SeedMalformed {
            given: given.to_owned(),
            reason: "the part before @ is not a node id",
        })?;
        if endpoint.is_empty() {
            return Err(Error::SeedMalformed {
                given: given.to_owned(),
                reason: "there is no address after the @",
            });
        }
        Ok(Self {
            node,
            endpoint: endpoint.to_owned(),
        })
    }
}

/// One credential file handed to [`Joining::parse`]: its bytes, and the path
/// they came from, which is used only to name the file in a refusal.
#[derive(Debug, Clone, Copy)]
pub struct CredentialFile<'a> {
    /// The file's contents.
    pub bytes: &'a [u8],
    /// Where the contents came from.
    pub path: &'a Path,
}

/// A cluster configuration, read.
#[derive(Debug)]
pub struct Joining {
    /// What this node presents to a peer, and what a peer's door asks about it.
    pub mine: Credential,
    /// The one root every peer in this cluster is issued by.
    pub authority: CertificateDer<'static>,
    /// Where this node's own peer door binds.
    ///
    /// Carried unparsed, because the one thing that can settle whether an
    /// address is usable is binding it, and that happens where the door is
    /// opened rather than here.
    pub door: String,
    /// Where to reach the cluster, until the catalog names a peer instead.
    ///
    /// Parsed here and not at the dial, so a mistyped seed stops the node at
    /// start rather than at the first round.
    pub seeds: Vec<Seed>,
}

impl Joining {
    /// Read the three files [`Told`] names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CredentialUnreadable`] when a file will not open,
    /// [`Error::CredentialEmpty`] when one opens and holds nothing of its kind,
    /// and [`Error::AuthorityNotSingle`] when the authority file holds any
    /// number of certificates other than one.
    /// [`Error::SeedMalformed`] when a `--seed` value is not
    /// `<node-id>@<host:port>`.
    pub fn read(told: &Told) -> Result<Self> {
        let chain = slurp(&told.chain, CHAIN)?;
        // Refused before it is read when others on the host may read it.
        let key = tessari_serve::tls::read_private_key(&told.key).map_err(|reason| {
            Error::CredentialUnreadable {
                part: KEY,
                path: shown(&told.key),
                reason,
            }
        })?;
        let authority = slurp(&told.authority, AUTHORITY)?;
        Self::parse(
            CredentialFile {
                bytes: &chain,
                path: &told.chain,
            },
            CredentialFile {
                bytes: &key,
                path: &told.key,
            },
            CredentialFile {
                bytes: &authority,
                path: &told.authority,
            },
            told.door.clone(),
            told.seeds.clone(),
        )
    }

    /// The same, from bytes already in hand.
    ///
    /// Every rule about what a credential must contain lives here rather than in
    /// [`Self::read`], so none of them needs a file to be tested. The paths come
    /// along only to be named in a refusal.
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn parse(
        chain: CredentialFile<'_>,
        key: CredentialFile<'_>,
        authority: CredentialFile<'_>,
        door: String,
        seeds: Vec<String>,
    ) -> Result<Self> {
        let mine = peer_credential(chain, key)?;
        let CredentialFile {
            bytes: authority,
            path: authority_at,
        } = authority;
        let found = certificates(authority, AUTHORITY, authority_at)?;
        // Exactly one, because the door trusts exactly one root. Two would leave
        // one of them silently ignored, and during a rotation the ignored one is
        // as likely to be the new authority as the old.
        let [authority] = <[CertificateDer<'static>; 1]>::try_from(found).map_err(|found| {
            Error::AuthorityNotSingle {
                path: shown(authority_at),
                found: found.len(),
            }
        })?;
        let seeds = seeds
            .iter()
            .map(|given| Seed::parse(given))
            .collect::<Result<Vec<Seed>>>()?;
        Ok(Self {
            mine,
            authority,
            door,
            seeds,
        })
    }
}

/// This node's peer credential from its two files: a chain of at least one
/// certificate, and a private key.
///
/// The rules start-up applies, in one place, so a credential re-read while the
/// node serves is judged exactly as the first one was (ADR-0108 D6).
///
/// # Errors
///
/// [`Error::CredentialUnreadable`] when a file is not PEM, and
/// [`Error::CredentialEmpty`] when one holds nothing of its kind.
pub fn peer_credential(chain: CredentialFile<'_>, key: CredentialFile<'_>) -> Result<Credential> {
    let CredentialFile {
        bytes: chain,
        path: chain_at,
    } = chain;
    let CredentialFile {
        bytes: key,
        path: key_at,
    } = key;
    let chain = certificates(chain, CHAIN, chain_at)?;
    if chain.is_empty() {
        return Err(Error::CredentialEmpty {
            part: CHAIN,
            path: shown(chain_at),
            wanted: "certificate",
        });
    }
    // The two refusals are distinct and the mapping is by VARIANT, not by a
    // catch-all: a well-formed file that holds no key is a CONTENT problem
    // and reports `CredentialEmpty`, while anything the parser could not
    // read at all is `CredentialUnreadable`. Collapsing them would send an
    // operator looking for a bad path when what they have is a bad file.
    let key = PrivateKeyDer::from_pem_slice(key).map_err(|why| match why {
        rustls::pki_types::pem::Error::NoItemsFound => Error::CredentialEmpty {
            part: KEY,
            path: shown(key_at),
            wanted: "private key",
        },
        why => Error::CredentialUnreadable {
            part: KEY,
            path: shown(key_at),
            reason: why.to_string(),
        },
    })?;
    Ok(Credential { chain, key })
}

/// Every certificate a PEM file holds, in the order it holds them.
fn certificates(pem: &[u8], part: &'static str, at: &Path) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|why| Error::CredentialUnreadable {
            part,
            path: shown(at),
            reason: why.to_string(),
        })
}

/// Read a file, or say which one and why not.
fn slurp(at: &Path, part: &'static str) -> Result<Vec<u8>> {
    std::fs::read(at).map_err(|why| Error::CredentialUnreadable {
        part,
        path: shown(at),
        reason: why.to_string(),
    })
}

/// A path as the operator typed it, for a message.
fn shown(at: &Path) -> String {
    at.display().to_string()
}

#[cfg(test)]
mod tests;
