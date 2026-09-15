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
        let key = slurp(&told.key, KEY)?;
        let authority = slurp(&told.authority, AUTHORITY)?;
        Self::parse(
            &chain,
            &told.chain,
            &key,
            &told.key,
            &authority,
            &told.authority,
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
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        chain: &[u8],
        chain_at: &Path,
        key: &[u8],
        key_at: &Path,
        authority: &[u8],
        authority_at: &Path,
        door: String,
        seeds: Vec<String>,
    ) -> Result<Self> {
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
            mine: Credential { chain, key },
            authority,
            door,
            seeds,
        })
    }
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
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A seed in the form the flag now takes, `<node-id>@<host:port>`.
    const ONE_SEED: &str = "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a@one.example:9080";
    const TWO_SEED: &str = "2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b@two.example:9080";

    /// A PEM authority and a PEM leaf it signed, minted in memory.
    ///
    /// In memory for the reason the peer link's own fixture gives: a fixture on
    /// disk is key material in a repository, and one with an expiry date is a
    /// test that fails on a day nobody chose.
    struct Pem {
        authority: String,
        leaf: String,
        key: String,
    }

    fn minted() -> Pem {
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let authority_key = rcgen::KeyPair::generate().unwrap();
        let authority = params.self_signed(&authority_key).unwrap();

        let leaf_params = rcgen::CertificateParams::new(vec!["a.peer.tessari".to_owned()]).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = leaf_params
            .signed_by(&leaf_key, &authority, &authority_key)
            .unwrap();

        Pem {
            authority: authority.pem(),
            leaf: leaf.pem(),
            key: leaf_key.serialize_pem(),
        }
    }

    /// Where a test node's own door would bind. Any address will do — nothing
    /// in this module binds one; that is `link.rs`'s and the binary's work.
    const DOOR: &str = "0.0.0.0:9081";

    fn at(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn parsed(pem: &Pem) -> Result<Joining> {
        Joining::parse(
            pem.leaf.as_bytes(),
            &at("leaf.pem"),
            pem.key.as_bytes(),
            &at("key.pem"),
            pem.authority.as_bytes(),
            &at("ca.pem"),
            DOOR.to_owned(),
            vec![ONE_SEED.to_owned()],
        )
    }

    #[test]
    fn a_node_told_none_of_it_is_a_node_that_is_not_in_a_cluster() {
        let told = Told::from_parts(None, None, None, None, Vec::new()).unwrap();
        assert!(
            told.is_none(),
            "absent is the unclustered node, not a failure"
        );
    }

    #[test]
    fn a_node_told_some_of_it_does_not_start() {
        let failure = Told::from_parts(
            Some(at("leaf.pem")),
            None,
            Some(at("ca.pem")),
            Some(DOOR.to_owned()),
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("half a cluster is refused");
        let said = failure.to_string();
        let (given, missing) = said
            .split_once("but not")
            .expect("the refusal separates what was given from what was missing");
        assert!(missing.contains("a private key"), "names what was missing");
        assert!(given.contains("a peer credential"), "names what was given");
    }

    #[test]
    fn a_cluster_that_names_no_seed_is_a_configuration_the_store_completes() {
        // The founding node, and the single-node deployment being clustered.
        // Both have somewhere to be reached and nowhere to be reached FROM, and
        // refusing them here refuses the one node that needs no seed at all
        // (Q-577). Whether this node can actually reach anybody is a question
        // about the seeds OR the catalog, and only `serve` can see both.
        let told = Told::from_parts(
            Some(at("leaf.pem")),
            Some(at("key.pem")),
            Some(at("ca.pem")),
            Some(DOOR.to_owned()),
            Vec::new(),
        )
        .expect("four parts and no seed is a cluster configuration")
        .expect("all four given");
        assert!(
            told.seeds.is_empty(),
            "no seed was named and none is invented"
        );
        assert_eq!(told.door, DOOR, "the address its own door binds");
    }

    #[test]
    fn a_seed_on_its_own_is_still_half_a_cluster() {
        // The other side of the same relaxation: a seed cannot COMPLETE a
        // configuration, so an operator who passed only `--seed` must be told
        // their four other flags never arrived rather than quietly starting a
        // node that is not in a cluster at all.
        let failure = Told::from_parts(None, None, None, None, vec![ONE_SEED.to_owned()])
            .expect_err("a seed alone is not a cluster configuration");
        let said = failure.to_string();
        let (given, missing) = said
            .split_once("but not")
            .expect("the refusal separates what was given from what was missing");
        assert!(given.contains("seed addresses"), "names what was given");
        assert!(
            missing.contains("a peer credential") && missing.contains("a peer address"),
            "names the parts that never arrived, not just the first of them"
        );
    }

    #[test]
    fn a_node_told_all_of_it_keeps_every_path_it_was_given() {
        let told = Told::from_parts(
            Some(at("leaf.pem")),
            Some(at("key.pem")),
            Some(at("ca.pem")),
            Some(DOOR.to_owned()),
            vec![ONE_SEED.to_owned(), TWO_SEED.to_owned()],
        )
        .unwrap()
        .expect("all five given");
        assert_eq!(told.chain, at("leaf.pem"));
        assert_eq!(told.key, at("key.pem"));
        assert_eq!(told.authority, at("ca.pem"));
        assert_eq!(told.door, DOOR, "the address its own door binds");
        assert_eq!(told.seeds.len(), 2, "both seeds, in the order given");
    }

    #[test]
    fn a_cluster_with_nowhere_to_be_reached_is_half_configured() {
        // The part an operator is likeliest to forget, because the other four
        // are all about reaching somebody ELSE and this one is about being
        // reachable. Told everything but this, a node would dial its seeds,
        // learn the cluster, and be a member nothing could ever call back.
        let failure = Told::from_parts(
            Some(at("leaf.pem")),
            Some(at("key.pem")),
            Some(at("ca.pem")),
            None,
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("a peer address is a part like the others");
        let said = failure.to_string();
        let (given, missing) = said
            .split_once("but not")
            .expect("the refusal separates what was given from what was missing");
        assert!(missing.contains("a peer address"), "names what was missing");
        assert!(given.contains("seed addresses"), "names what was given");
    }

    #[test]
    fn a_credential_that_reads_carries_its_chain_its_key_and_one_authority() {
        let pem = minted();
        let joining = parsed(&pem).expect("a well-formed configuration");
        assert_eq!(joining.mine.chain.len(), 1, "the leaf");
        assert_eq!(
            joining.seeds,
            vec![Seed {
                node: [0x1a; NODE_ID_LEN],
                endpoint: "one.example:9080".to_owned(),
            }],
            "the seed is held as the pair it names, not as the text it was given"
        );
        assert!(
            !joining.authority.as_ref().is_empty(),
            "the authority's own bytes"
        );
    }

    #[test]
    fn a_credential_file_holding_no_certificate_is_refused_by_content_not_by_path() {
        let pem = minted();
        let failure = Joining::parse(
            b"this file exists and is not a certificate\n",
            &at("leaf.pem"),
            pem.key.as_bytes(),
            &at("key.pem"),
            pem.authority.as_bytes(),
            &at("ca.pem"),
            DOOR.to_owned(),
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("an empty chain is not a credential");
        let said = failure.to_string();
        assert!(said.contains("leaf.pem"), "names the file");
        assert!(said.contains("held no certificate"), "a content problem");
    }

    #[test]
    fn a_key_file_holding_no_key_is_refused_and_the_refusal_quotes_nothing() {
        // The authority's own certificate stands in the key slot deliberately.
        // It is a WELL-FORMED PEM that holds no private key, so the parser skips
        // the section it cannot use, finds nothing, and answers
        // `Err(NoItemsFound)` — which the mapping turns into the "held no
        // private key" refusal that is the one under test. Any other parse error
        // takes the `CredentialUnreadable` branch and never reaches it.
        let pem = minted();
        let failure = Joining::parse(
            pem.leaf.as_bytes(),
            &at("leaf.pem"),
            pem.authority.as_bytes(),
            &at("key.pem"),
            pem.authority.as_bytes(),
            &at("ca.pem"),
            DOOR.to_owned(),
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("a file with no key in it is not a key");
        let said = failure.to_string();
        assert!(said.contains("key.pem"), "names the file");
        assert!(said.contains("held no private key"), "refuses on content");
        assert!(
            !said.contains("BEGIN"),
            "a refusal about a key never quotes the file it read"
        );
    }

    #[test]
    fn a_certificate_file_that_will_not_parse_is_refused_rather_than_read_as_empty() {
        // `certificates` is shared by the chain and the authority, and its error
        // branch had no test either: a file with no certificate in it and a file
        // whose certificate will not decode both ended at `chain.is_empty()`,
        // which reports the wrong one. A section that opens and will not decode
        // is unreadable, not absent.
        let pem = minted();
        let failure = Joining::parse(
            b"-----BEGIN CERTIFICATE-----\n@@@@@@@@\n-----END CERTIFICATE-----\n",
            &at("leaf.pem"),
            pem.key.as_bytes(),
            &at("key.pem"),
            pem.authority.as_bytes(),
            &at("ca.pem"),
            DOOR.to_owned(),
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("a certificate that will not decode is not a certificate");
        let said = failure.to_string();
        assert!(said.contains("leaf.pem"), "names the file");
        assert!(
            !said.contains("held no certificate"),
            "not the content refusal: the file held a section, it just would not read"
        );
    }

    #[test]
    fn a_key_file_that_will_not_parse_is_a_different_refusal_and_still_quotes_nothing() {
        // The other half of the key mapping, and until W243 nothing exercised
        // it: every fixture fed the parser PEM that was absent rather than PEM
        // that was broken, so the two refusals were one tested branch and one
        // argument. A section that opens and then holds nothing decodable is a
        // file the parser could not READ, which is a different thing to tell an
        // operator than a file that held nothing of its kind.
        //
        // The body has to be outside the base64 alphabet to reach this branch,
        // and the first draft of this test did not know that. `not base64 at
        // all` is, letter for letter, valid base64, and this layer DECODES
        // rather than validates — so it parsed cheerfully into a `Pkcs8` key of
        // fifteen meaningless bytes and the test failed by succeeding. Whether a
        // key is a key is settled at the handshake, not here, and that was as
        // true of the parser this wave removed.
        let pem = minted();
        let failure = Joining::parse(
            pem.leaf.as_bytes(),
            &at("leaf.pem"),
            b"-----BEGIN PRIVATE KEY-----\n@@@@@@@@\n-----END PRIVATE KEY-----\n",
            &at("key.pem"),
            pem.authority.as_bytes(),
            &at("ca.pem"),
            DOOR.to_owned(),
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("a key that will not decode is not a key");
        let said = failure.to_string();
        assert!(said.contains("key.pem"), "names the file");
        assert!(
            !said.contains("held no private key"),
            "not the content refusal: the file held a section, it just would not read"
        );
        assert!(
            !said.contains("BEGIN") && !said.contains("@@@"),
            "a refusal about a key never quotes the file it read, however it failed"
        );
    }

    #[test]
    fn an_authority_file_holding_two_certificates_is_refused_rather_than_half_trusted() {
        let pem = minted();
        let two = format!("{}{}", pem.authority, pem.authority);
        let failure = Joining::parse(
            pem.leaf.as_bytes(),
            &at("leaf.pem"),
            pem.key.as_bytes(),
            &at("key.pem"),
            two.as_bytes(),
            &at("ca.pem"),
            DOOR.to_owned(),
            vec![ONE_SEED.to_owned()],
        )
        .expect_err("the door trusts exactly one root");
        let said = failure.to_string();
        assert!(said.contains("2 certificates"), "says how many were found");
        assert!(said.contains("ca.pem"), "names the file");
    }

    #[test]
    fn a_seed_is_the_node_and_the_address_together() {
        let seed = Seed::parse(ONE_SEED).expect("a well-formed seed");
        assert_eq!(seed.node, [0x1a; NODE_ID_LEN], "the id before the @");
        assert_eq!(seed.endpoint, "one.example:9080", "the address after it");
    }

    #[test]
    fn a_seed_reads_the_hyphenated_form_of_an_id_too() {
        // `INFO FOR NODE` prints one form and a person copying an id out of a
        // ticket may paste the other. Both are the same sixteen bytes, and a
        // refusal that depends on which one was pasted would be a refusal about
        // punctuation dressed as a refusal about identity.
        let hyphenated = "1a1a1a1a-1a1a-1a1a-1a1a-1a1a1a1a1a1a@one.example:9080";
        let seed = Seed::parse(hyphenated).expect("the hyphenated form is an id");
        assert_eq!(seed.node, [0x1a; NODE_ID_LEN]);
    }

    #[test]
    fn a_seed_that_is_only_an_address_is_refused_at_start() {
        // The form the flag carried until ADR-0067. It cannot be dialled — the
        // handshake derives the peer's name from its id — so it is refused here
        // rather than at the first round, where it would arrive as a TLS
        // failure and read like a certificate problem.
        let refused = Seed::parse("one.example:9080").expect_err("no id, no dial");
        let said = refused.to_string();
        assert!(said.contains("one.example:9080"), "quotes what was given");
        assert!(said.contains("no @"), "names which half is missing: {said}");
    }

    #[test]
    fn a_seed_whose_id_is_not_an_id_is_refused_at_start() {
        let refused = Seed::parse("not-an-id@one.example:9080").expect_err("that is no id");
        assert!(
            refused.to_string().contains("not a node id"),
            "names the half that was wrong, not the whole value"
        );
    }

    #[test]
    fn a_seed_with_an_id_and_no_address_is_refused_at_start() {
        let given = format!("{}@", "1a".repeat(NODE_ID_LEN));
        let refused = Seed::parse(&given).expect_err("nowhere to dial");
        assert!(
            refused.to_string().contains("no address after"),
            "an id on its own is not a seed"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_named_in_the_refusal() {
        let told = Told {
            chain: at("/nowhere/that/exists/leaf.pem"),
            key: at("/nowhere/that/exists/key.pem"),
            authority: at("/nowhere/that/exists/ca.pem"),
            door: DOOR.to_owned(),
            seeds: vec![ONE_SEED.to_owned()],
        };
        let failure = Joining::read(&told).expect_err("nothing to read");
        assert!(
            failure
                .to_string()
                .contains("/nowhere/that/exists/leaf.pem"),
            "names the path the operator gave"
        );
    }
}
