//! TLS on the client surfaces (ADR-0108 D4).
//!
//! Both surfaces take the same certificate and key, and a refusal must name the
//! same part of the same file whichever surface read it, so the reading lives
//! here rather than once per surface. What each surface does with the result —
//! the wire node wraps a socket, HTTP wraps a listener — stays with it.
//!
//! TLS 1.2 and 1.3 only, with rustls's own cipher suites: there is no setting
//! that widens either, because a legacy suite enabled for one old client is
//! offered to every client, including one that downgrades on purpose.

use std::sync::Arc;

use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig};

/// Why a certificate, key or authority could not be used.
///
/// Each names the part and the file, because an operator holding three PEM
/// files needs to know which one to look at.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Refused {
    /// The file could not be read, or is not PEM.
    #[error("the TLS {part} at {path} could not be read: {reason}")]
    Unreadable {
        /// Which file: the certificate, the key or the authority.
        part: &'static str,
        /// Where it was read from.
        path: String,
        /// What the reader said.
        reason: String,
    },
    /// The file is PEM and holds nothing of the kind asked for.
    #[error("the TLS {part} at {path} holds no {wanted}")]
    Empty {
        /// Which file.
        part: &'static str,
        /// Where it was read from.
        path: String,
        /// What it should have held.
        wanted: &'static str,
    },
    /// The certificate and the key are each well formed and do not belong
    /// together, or the key is of a kind this build cannot sign with.
    #[error("the TLS certificate and key do not make a credential: {reason}")]
    Mismatched {
        /// What rustls said about the pair.
        reason: String,
    },
}

/// One PEM file's bytes and where they came from, for the refusal.
#[derive(Debug, Clone, Copy)]
pub struct Pem<'a> {
    /// What the file holds.
    pub bytes: &'a [u8],
    /// Where it was read from, named in a refusal and nowhere else.
    pub path: &'a str,
}

impl Pem<'_> {
    /// Every certificate the file holds, in its order; at least one.
    fn certificates(self, part: &'static str) -> Result<Vec<CertificateDer<'static>>, Refused> {
        let found = CertificateDer::pem_slice_iter(self.bytes)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|why| self.unreadable(part, &why))?;
        if found.is_empty() {
            return Err(Refused::Empty {
                part,
                path: self.path.to_owned(),
                wanted: "certificate",
            });
        }
        Ok(found)
    }

    fn unreadable(self, part: &'static str, why: &pem::Error) -> Refused {
        Refused::Unreadable {
            part,
            path: self.path.to_owned(),
            reason: why.to_string(),
        }
    }
}

/// The server side of a client surface: this node's certificate chain (leaf
/// first) and its private key, offering `alpn` in order of preference.
///
/// The key is checked against the leaf here, at start-up, so a pair that does
/// not belong together refuses the node rather than every client's handshake.
///
/// # Errors
///
/// [`Refused`] naming the part that could not be used.
pub fn server_config(
    chain: Pem<'_>,
    key: Pem<'_>,
    alpn: &[&[u8]],
) -> Result<Arc<ServerConfig>, Refused> {
    let certificates = chain.certificates("certificate")?;
    let private = PrivateKeyDer::from_pem_slice(key.bytes).map_err(|why| match why {
        pem::Error::NoItemsFound => Refused::Empty {
            part: "key",
            path: key.path.to_owned(),
            wanted: "private key",
        },
        why => key.unreadable("key", &why),
    })?;
    let mut settings = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private)
        .map_err(|why| Refused::Mismatched {
            reason: why.to_string(),
        })?;
    settings.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    Ok(Arc::new(settings))
}

/// The certificates a client trusts a node by: every certificate in the file.
///
/// # Errors
///
/// [`Refused`] when the file holds no certificate, or one that cannot be a
/// trust anchor.
pub fn authority(file: Pem<'_>) -> Result<RootCertStore, Refused> {
    let mut roots = RootCertStore::empty();
    for certificate in file.certificates("authority")? {
        roots.add(certificate).map_err(|why| Refused::Unreadable {
            part: "authority",
            path: file.path.to_owned(),
            reason: why.to_string(),
        })?;
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::{Pem, Refused, authority, server_config};

    /// A self-signed leaf for `localhost`, minted here so no key is ever kept.
    fn minted() -> (String, String) {
        let made = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("a certificate");
        (made.cert.pem(), made.key_pair.serialize_pem())
    }

    fn pem<'a>(bytes: &'a str, path: &'a str) -> Pem<'a> {
        Pem {
            bytes: bytes.as_bytes(),
            path,
        }
    }

    #[test]
    fn a_certificate_and_its_key_make_a_server_and_offer_what_was_asked() {
        let (certificate, key) = minted();
        let settings = server_config(
            pem(&certificate, "cert.pem"),
            pem(&key, "key.pem"),
            &[b"http/1.1"],
        )
        .expect("a usable credential");
        assert_eq!(settings.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn each_part_that_cannot_be_used_is_named_with_its_file() {
        let (certificate, key) = minted();
        let (_, someone_elses) = minted();

        let no_certificate = server_config(pem(&key, "cert.pem"), pem(&key, "key.pem"), &[])
            .expect_err("a key is not a certificate");
        assert!(
            matches!(&no_certificate, Refused::Empty { part: "certificate", path, .. } if path == "cert.pem"),
            "{no_certificate}"
        );

        let no_key = server_config(
            pem(&certificate, "cert.pem"),
            pem(&certificate, "key.pem"),
            &[],
        )
        .expect_err("a certificate is not a key");
        assert!(
            matches!(&no_key, Refused::Empty { part: "key", path, .. } if path == "key.pem"),
            "{no_key}"
        );

        let mismatched = server_config(
            pem(&certificate, "cert.pem"),
            pem(&someone_elses, "key.pem"),
            &[],
        )
        .expect_err("another certificate's key");
        assert!(
            matches!(mismatched, Refused::Mismatched { .. }),
            "{mismatched}"
        );
    }

    #[test]
    fn an_authority_is_every_certificate_in_its_file_and_never_none() {
        let (first, _) = minted();
        let (second, _) = minted();
        let both = format!("{first}{second}");
        assert_eq!(authority(pem(&both, "ca.pem")).expect("two roots").len(), 2);
        let refused = authority(pem("", "ca.pem")).expect_err("an empty file");
        assert!(
            matches!(
                &refused,
                Refused::Empty {
                    part: "authority",
                    ..
                }
            ),
            "{refused}"
        );
    }
}
