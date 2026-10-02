//! Credential files re-read while the node serves (ADR-0108 D6).
//!
//! A renewed certificate is a file that changed, and the node notices on its
//! own: every [`LOOK_SECONDS`] each watched pair is read, and a pair whose bytes
//! differ from the last ones judged is handed to its credential, which checks
//! it whole before presenting it. New connections get the new certificate; open
//! ones finish on the old.
//!
//! # By content, not by modification time
//!
//! A digest of both files rather than their times: a copy that preserves the
//! time, or a clock that stepped back, would hide a change a time comparison
//! relies on, and two small files are cheap to read. Only the digest is kept,
//! so no second copy of a private key sits in memory to compare against.
//!
//! # A pair written in two steps
//!
//! An operator renews the certificate, then the key. Between the two writes the
//! pair does not belong together, the replacement is refused, and the old
//! credential stays — which is right, and is said once: a refused pair is
//! remembered, so the same two files are not refused again every few seconds.
//! The next write changes the digest and the pair is judged afresh.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// How often the files are looked at.
pub(crate) const LOOK_SECONDS: u64 = 2;

/// Puts a pair of files' bytes into service, or says why it would not.
type Apply = Box<dyn Fn(&[u8], &[u8]) -> Result<(), String> + Send>;

/// One pair of credential files and the credential they replace.
pub(crate) struct Watched {
    /// Which credential, for the log line: the client surfaces' or the peers'.
    surface: &'static str,
    chain: PathBuf,
    key: PathBuf,
    /// The digest of the pair last judged, whatever the judgement was.
    seen: Option<[u8; 32]>,
    apply: Apply,
}

/// What one look found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Looked {
    /// The same bytes as last time.
    Unchanged,
    /// New bytes, now presented.
    Replaced,
    /// New bytes that do not make a credential; the old one stays.
    Refused(String),
    /// A file could not be read; the old credential stays and the pair is
    /// looked at again next time.
    Unreadable(String),
}

impl Watched {
    /// Watch `chain` and `key`, whose current contents are already in service.
    ///
    /// The first look records them rather than replacing anything, so a node
    /// that just started does not log a reload of what it started with.
    pub(crate) fn new(surface: &'static str, chain: PathBuf, key: PathBuf, apply: Apply) -> Self {
        let mut watched = Self {
            surface,
            chain,
            key,
            seen: None,
            apply,
        };
        if let Ok((chain, key)) = watched.read() {
            watched.seen = Some(digest(&chain, &key));
        }
        watched
    }

    /// The client surfaces' certificate.
    pub(crate) fn clients(
        credential: tessari_serve::tls::Credential,
        chain: PathBuf,
        key: PathBuf,
    ) -> Self {
        let (chain_at, key_at) = (chain.display().to_string(), key.display().to_string());
        Self::new(
            "client",
            chain,
            key,
            Box::new(move |chain, key| {
                credential
                    .replace(
                        tessari_serve::tls::Pem {
                            bytes: chain,
                            path: &chain_at,
                        },
                        tessari_serve::tls::Pem {
                            bytes: key,
                            path: &key_at,
                        },
                    )
                    .map_err(|refused| refused.to_string())
            }),
        )
    }

    /// The peer link's certificate.
    pub(crate) fn peers(keys: tessari_wire::PeerKeys, told: &tessari_wire::Told) -> Self {
        let (chain_at, key_at) = (told.chain.clone(), told.key.clone());
        Self::new(
            "peer",
            told.chain.clone(),
            told.key.clone(),
            Box::new(move |chain, key| {
                let mine = tessari_wire::peer_credential(
                    tessari_wire::CredentialFile {
                        bytes: chain,
                        path: &chain_at,
                    },
                    tessari_wire::CredentialFile {
                        bytes: key,
                        path: &key_at,
                    },
                )
                .map_err(|refused| refused.to_string())?;
                keys.replace(mine).map_err(|refused| refused.to_string())
            }),
        )
    }

    /// Read the pair, and put it into service when it changed.
    pub(crate) fn look(&mut self) -> Looked {
        let (chain, key) = match self.read() {
            Ok(read) => read,
            Err(why) => return Looked::Unreadable(why),
        };
        let now = digest(&chain, &key);
        if self.seen == Some(now) {
            return Looked::Unchanged;
        }
        self.seen = Some(now);
        match (self.apply)(&chain, &key) {
            Ok(()) => Looked::Replaced,
            Err(why) => Looked::Refused(why),
        }
    }

    /// Look, and say what happened when something did.
    pub(crate) fn look_and_say(&mut self) {
        match self.look() {
            Looked::Unchanged => {}
            Looked::Replaced => log::info!(
                "the {} certificate at {} was renewed; new connections present it",
                self.surface,
                self.chain.display()
            ),
            Looked::Refused(why) => log::warn!(
                "the {} certificate files changed and were not taken ({why}); \
                 the previous certificate is still presented",
                self.surface
            ),
            Looked::Unreadable(why) => log::warn!(
                "the {} certificate files could not be read ({why}); \
                 the previous certificate is still presented",
                self.surface
            ),
        }
    }

    fn read(&self) -> Result<(Vec<u8>, Vec<u8>), String> {
        let read = |path: &PathBuf| {
            std::fs::read(path).map_err(|why| format!("{}: {why}", path.display()))
        };
        Ok((read(&self.chain)?, read(&self.key)?))
    }
}

/// Both files' bytes, each length-prefixed so two pairs that concatenate to
/// the same bytes cannot share a digest.
fn digest(chain: &[u8], key: &[u8]) -> [u8; 32] {
    let mut hashing = Sha256::new();
    for part in [chain, key] {
        // A file longer than `u64::MAX` bytes cannot have been read.
        hashing.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        hashing.update(part);
    }
    hashing.finalize().into()
}

/// The peer link's copy of the catalog's revocation list (ADR-0108 D6).
///
/// The catalog is the list and the keys refuse from a copy of it, refreshed
/// whole on every look, so a row that reaches this node by the log is refused
/// within [`LOOK_SECONDS`] — and a list that cannot be read leaves the last
/// copy in force rather than an empty one.
pub(crate) struct Revoking {
    pub(crate) db: std::sync::Arc<tessaridb::Db>,
    pub(crate) keys: tessari_wire::PeerKeys,
}

impl Revoking {
    /// Hand the keys the list the catalog holds now.
    ///
    /// # Errors
    ///
    /// The store's, when the list cannot be read; the keys are left as they were.
    pub(crate) fn refresh(&self) -> Result<(), String> {
        let listed = self
            .db
            .store()
            .begin()
            .and_then(|mut transaction| {
                tessari_storage::Catalog::new(&mut transaction).revoked_certificates()
            })
            .map_err(|why| why.to_string())?;
        let listed: tessari_wire::Revoked = listed.into_iter().collect();
        if listed != self.keys.refusing() {
            log::info!(
                "the peer link now refuses {} revoked certificate(s)",
                listed.len()
            );
            self.keys.refuse(listed);
        }
        Ok(())
    }
}

/// Look at every watched pair, and the revocation list when there is one,
/// every [`LOOK_SECONDS`] until `stop`.
///
/// The reads run on the blocking pool, which is where [`tessari_wire::every`]
/// runs each pass.
pub(crate) async fn watch(
    mut watched: Vec<Watched>,
    revoking: Option<Revoking>,
    stop: tokio_util::sync::CancellationToken,
) {
    tessari_wire::every(
        std::time::Duration::from_secs(LOOK_SECONDS),
        &stop,
        move |_| {
            for pair in &mut watched {
                pair.look_and_say();
            }
            if let Some(Err(why)) = revoking.as_ref().map(Revoking::refresh) {
                log::warn!("the revocation list could not be read ({why}); the last one stands");
            }
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{Looked, Watched};

    /// A pair of files in a fresh directory, and what was put into service.
    fn watched(dir: &tempfile::TempDir) -> (Watched, Arc<Mutex<Vec<String>>>) {
        let (chain, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&chain, "first chain").expect("a chain file");
        std::fs::write(&key, "first key").expect("a key file");
        let taken = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&taken);
        let pair = Watched::new(
            "client",
            chain,
            key,
            Box::new(move |chain, key| {
                if String::from_utf8_lossy(key).contains("broken") {
                    return Err("the key is not the certificate's".to_owned());
                }
                into.lock()
                    .expect("the record")
                    .push(String::from_utf8_lossy(chain).into_owned());
                Ok(())
            }),
        );
        (pair, taken)
    }

    fn taken(record: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        record.lock().expect("the record").clone()
    }

    /// Peer keys on a credential minted for the length of one test.
    fn keys() -> tessari_wire::PeerKeys {
        let key = rcgen::KeyPair::generate().expect("a key");
        let certificate = rcgen::CertificateParams::new(vec!["n.peer.tessari".to_owned()])
            .expect("parameters")
            .self_signed(&key)
            .expect("a certificate");
        tessari_wire::PeerKeys::new(
            tessari_wire::Credential {
                chain: vec![certificate.der().clone()],
                key: rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der())
                    .expect("a key in DER"),
            },
            certificate.der().clone(),
        )
        .expect("usable keys")
    }

    #[test]
    fn the_peer_link_refuses_what_the_catalog_revoked_and_nothing_else() {
        let db = Arc::new(tessaridb::Db::in_memory().expect("a store"));
        let revoking = super::Revoking {
            db: Arc::clone(&db),
            keys: keys(),
        };
        revoking.refresh().expect("an empty list");
        assert!(revoking.keys.refusing().is_empty());

        let fingerprint = "ab".repeat(32);
        db.session()
            .run(&format!("REVOKE CERTIFICATE '{fingerprint}';"))
            .expect("a revocation");
        revoking.refresh().expect("the list");
        assert_eq!(
            revoking.keys.refusing(),
            tessari_wire::Revoked::from([fingerprint])
        );
    }

    #[test]
    fn what_the_node_started_with_is_not_reloaded() {
        let dir = tempfile::tempdir().expect("a directory");
        let (mut pair, record) = watched(&dir);
        assert_eq!(pair.look(), Looked::Unchanged);
        assert!(taken(&record).is_empty());
    }

    #[test]
    fn a_changed_pair_is_put_into_service_once() {
        let dir = tempfile::tempdir().expect("a directory");
        let (mut pair, record) = watched(&dir);
        std::fs::write(dir.path().join("cert.pem"), "second chain").expect("a renewal");
        assert_eq!(pair.look(), Looked::Replaced);
        assert_eq!(
            pair.look(),
            Looked::Unchanged,
            "the same bytes are not taken twice"
        );
        assert_eq!(taken(&record), vec!["second chain"]);
    }

    #[test]
    fn a_pair_refused_is_said_once_and_judged_again_when_it_changes() {
        let dir = tempfile::tempdir().expect("a directory");
        let (mut pair, record) = watched(&dir);
        std::fs::write(dir.path().join("cert.pem"), "second chain").expect("half a renewal");
        std::fs::write(dir.path().join("key.pem"), "broken key").expect("half a renewal");
        assert!(
            matches!(pair.look(), Looked::Refused(why) if why.contains("not the certificate's"))
        );
        assert_eq!(
            pair.look(),
            Looked::Unchanged,
            "refused once, not every look"
        );

        std::fs::write(dir.path().join("key.pem"), "second key").expect("the rest of it");
        assert_eq!(pair.look(), Looked::Replaced);
        assert_eq!(taken(&record), vec!["second chain"]);
    }

    #[test]
    fn a_file_that_cannot_be_read_keeps_the_credential_and_is_tried_again() {
        let dir = tempfile::tempdir().expect("a directory");
        let (mut pair, record) = watched(&dir);
        std::fs::remove_file(dir.path().join("key.pem")).expect("a key moved away");
        assert!(matches!(pair.look(), Looked::Unreadable(_)));
        std::fs::write(dir.path().join("key.pem"), "second key").expect("the key back");
        assert_eq!(pair.look(), Looked::Replaced);
        assert_eq!(taken(&record), vec!["first chain"]);
    }
}
