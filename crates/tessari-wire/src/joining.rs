//! What a node was told about the cluster it belongs to.
//!
//! # Configuration, not data, and the concept settled it
//!
//! A node's identity and roles come from its own store — `NodeIdentity` is
//! generated on first open and stable across restarts. Its declared membership
//! comes from the catalog, where `ReplicaDefinition` already keeps it. What
//! arrives here is the third thing: the **peer credential**, the **cluster
//! authority** and the **seed addresses** to dial for a first contact.
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

use std::io::BufReader;
use std::path::{Path, PathBuf};

use rustls::pki_types::CertificateDer;

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
    /// Addresses to dial for a first contact.
    pub seeds: Vec<String>,
}

impl Told {
    /// What four flags amount to: all of it, none of it, or a refusal.
    ///
    /// A seed list is a part like the other three, and an empty one is absence
    /// rather than emptiness — a cluster runs three to seven voting members, so
    /// one address is a single point of failure at the moment a node most needs
    /// to succeed, and none is not a configuration at all.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ClusterHalfConfigured`] when some parts were given and
    /// others were not, naming both sides.
    pub fn from_parts(
        chain: Option<PathBuf>,
        key: Option<PathBuf>,
        authority: Option<PathBuf>,
        seeds: Vec<String>,
    ) -> Result<Option<Self>> {
        let present = [
            ("a peer credential", chain.is_some()),
            ("a private key", key.is_some()),
            ("a cluster authority", authority.is_some()),
            ("seed addresses", !seeds.is_empty()),
        ];
        let given: Vec<&str> = present.iter().filter(|p| p.1).map(|p| p.0).collect();
        let missing: Vec<&str> = present.iter().filter(|p| !p.1).map(|p| p.0).collect();
        if given.is_empty() {
            return Ok(None);
        }
        // Destructured together rather than unwrapped one at a time: all four are
        // present exactly when nothing is missing, and asking each `Option` again
        // would be a second statement of a fact this list already carries.
        let (Some(chain), Some(key), Some(authority)) = (chain, key, authority) else {
            return Err(Error::ClusterHalfConfigured {
                given: given.join(", "),
                missing: missing.join(", "),
            });
        };
        if missing.is_empty() {
            Ok(Some(Self {
                chain,
                key,
                authority,
                seeds,
            }))
        } else {
            Err(Error::ClusterHalfConfigured {
                given: given.join(", "),
                missing: missing.join(", "),
            })
        }
    }
}

/// A cluster configuration, read.
#[derive(Debug)]
pub struct Joining {
    /// What this node presents to a peer, and what a peer's door asks about it.
    pub mine: Credential,
    /// The one root every peer in this cluster is issued by.
    pub authority: CertificateDer<'static>,
    /// Addresses to dial for a first contact.
    pub seeds: Vec<String>,
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
        let key = rustls_pemfile::private_key(&mut BufReader::new(key))
            .map_err(|why| Error::CredentialUnreadable {
                part: KEY,
                path: shown(key_at),
                reason: why.to_string(),
            })?
            .ok_or_else(|| Error::CredentialEmpty {
                part: KEY,
                path: shown(key_at),
                wanted: "private key",
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
        Ok(Self {
            mine: Credential { chain, key },
            authority,
            seeds,
        })
    }
}

/// Every certificate a PEM file holds, in the order it holds them.
fn certificates(pem: &[u8], part: &'static str, at: &Path) -> Result<Vec<CertificateDer<'static>>> {
    rustls_pemfile::certs(&mut BufReader::new(pem))
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
            vec!["one.example:9080".to_owned()],
        )
    }

    #[test]
    fn a_node_told_none_of_it_is_a_node_that_is_not_in_a_cluster() {
        let told = Told::from_parts(None, None, None, Vec::new()).unwrap();
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
            vec!["one.example:9080".to_owned()],
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
    fn a_cluster_with_no_seed_to_dial_is_half_configured() {
        let failure = Told::from_parts(
            Some(at("leaf.pem")),
            Some(at("key.pem")),
            Some(at("ca.pem")),
            Vec::new(),
        )
        .expect_err("a seed list is a part like the others");
        assert!(
            failure.to_string().contains("seed addresses"),
            "names the seeds"
        );
    }

    #[test]
    fn a_node_told_all_of_it_keeps_every_path_it_was_given() {
        let told = Told::from_parts(
            Some(at("leaf.pem")),
            Some(at("key.pem")),
            Some(at("ca.pem")),
            vec!["one.example:9080".to_owned(), "two.example:9080".to_owned()],
        )
        .unwrap()
        .expect("all four given");
        assert_eq!(told.chain, at("leaf.pem"));
        assert_eq!(told.key, at("key.pem"));
        assert_eq!(told.authority, at("ca.pem"));
        assert_eq!(told.seeds.len(), 2, "both seeds, in the order given");
    }

    #[test]
    fn a_credential_that_reads_carries_its_chain_its_key_and_one_authority() {
        let pem = minted();
        let joining = parsed(&pem).expect("a well-formed configuration");
        assert_eq!(joining.mine.chain.len(), 1, "the leaf");
        assert_eq!(joining.seeds, vec!["one.example:9080".to_owned()]);
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
            vec!["one.example:9080".to_owned()],
        )
        .expect_err("an empty chain is not a credential");
        let said = failure.to_string();
        assert!(said.contains("leaf.pem"), "names the file");
        assert!(said.contains("held no certificate"), "a content problem");
    }

    #[test]
    fn a_key_file_holding_no_key_is_refused_and_the_refusal_quotes_nothing() {
        // The authority's own certificate stands in the key slot deliberately.
        // It is a WELL-FORMED PEM that holds no private key, so the parser
        // answers `Ok(None)` and the "held no private key" refusal is the one
        // under test. Malformed bytes answer `Err` instead and never reach it.
        let pem = minted();
        let failure = Joining::parse(
            pem.leaf.as_bytes(),
            &at("leaf.pem"),
            pem.authority.as_bytes(),
            &at("key.pem"),
            pem.authority.as_bytes(),
            &at("ca.pem"),
            vec!["one.example:9080".to_owned()],
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
            vec!["one.example:9080".to_owned()],
        )
        .expect_err("the door trusts exactly one root");
        let said = failure.to_string();
        assert!(said.contains("2 certificates"), "says how many were found");
        assert!(said.contains("ca.pem"), "names the file");
    }

    #[test]
    fn a_path_that_does_not_exist_is_named_in_the_refusal() {
        let told = Told {
            chain: at("/nowhere/that/exists/leaf.pem"),
            key: at("/nowhere/that/exists/key.pem"),
            authority: at("/nowhere/that/exists/ca.pem"),
            seeds: vec!["one.example:9080".to_owned()],
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
