//! What a certificate is asked, and why it is never read.
//!
//! # Asking is not parsing, and the difference is the whole design
//!
//! Turning a peer's certificate into [`Presented`] looks like a parsing problem:
//! reach into the subject alternative name, split it, believe the bytes. Done
//! that way it costs an ASN.1 parser and puts a database in the business of
//! reading certificate internals — one of the oldest sources of remotely
//! reachable bugs there is.
//!
//! It is also the wrong shape, and [`crate::peer`] already said why: **the frame
//! claims and the credential is interrogated**. A claim does not need to be
//! extracted. It needs a question, and *is this certificate valid for this name*
//! is the question every TLS client on earth already asks about a hostname — so
//! the code that answers it is audited, is in this tree already, and is not ours.
//!
//! # The name a credential carries
//!
//! `<node id>.<purpose>.tessari`, and the purpose is in the name rather than
//! beside it because a credential that names a node without saying what it was
//! issued **for** is a credential that can be moved between links. A client's
//! certificate and a peer's certificate for the same node are then two different
//! names, and the one cannot answer for the other.
//!
//! # What this can say, and what it cannot
//!
//! Asking answers *does this credential name the node the frame claims* and
//! never *which node does it name instead*. When the answer is no, the refusal
//! therefore carries the certificate's **fingerprint** rather than some other
//! node id — which is the more useful half anyway. An operator holding a
//! mis-issued credential has to find the file, and a fingerprint names exactly
//! one file on exactly one machine, where an id names a machine that may well
//! not be the one holding it.

use rustls::pki_types::{CertificateDer, ServerName};
use sha2::{Digest, Sha256};

use tessari_encoding::NODE_ID_LEN;
use tessari_types::RecordId;

use crate::error::{Error, Result};
use crate::peer::{Presented, Purpose};

/// The suffix every credential in this cluster's private namespace ends in.
///
/// It is never resolved and never looked up. It is here because a name without
/// one is a bare label, and a bare label is the shape most likely to collide
/// with a name that means something to somebody else's resolver.
const REALM: &str = "tessari";

/// The name a credential must carry to speak for `node` in `purpose`.
#[must_use]
pub fn names(node: [u8; NODE_ID_LEN], purpose: Purpose) -> String {
    let part = match purpose {
        Purpose::Peer => "peer",
        Purpose::Client => "client",
    };
    format!("{}.{part}.{REALM}", RecordId::Uuid(node))
}

/// What the transport proved, given what the frame claims.
///
/// # Errors
///
/// Returns [`Error::Unidentified`] when nothing was presented at all, and
/// [`Error::CredentialNamesAnother`] when a credential was presented, was
/// accepted by the transport, and does not name `claimed` in either purpose.
pub fn presented(
    certificate: Option<&CertificateDer<'_>>,
    claimed: [u8; NODE_ID_LEN],
) -> Result<Presented> {
    let certificate = certificate.ok_or(Error::Unidentified)?;
    // Peer first: it is the answer this link is looking for, and asking for it
    // first means the common case costs one question.
    for purpose in [Purpose::Peer, Purpose::Client] {
        if valid_for(certificate, &names(claimed, purpose)) {
            return Ok(Presented {
                node: claimed,
                purpose,
            });
        }
    }
    Err(Error::CredentialNamesAnother {
        said: RecordId::Uuid(claimed).to_string(),
        fingerprint: fingerprint(certificate),
    })
}

/// The certificate's SHA-256, lowercase hex.
#[must_use]
pub fn fingerprint(certificate: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(certificate.as_ref());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Ask the certificate whether it speaks for this name.
///
/// A malformed certificate and an unparseable name both answer *no*, because
/// both mean the same thing here: this credential does not carry that name.
fn valid_for(certificate: &CertificateDer<'_>, name: &str) -> bool {
    let Ok(end_entity) = webpki::EndEntityCert::try_from(certificate) else {
        return false;
    };
    let Ok(subject) = ServerName::try_from(name) else {
        return false;
    };
    end_entity
        .verify_is_valid_for_subject_name(&subject)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credentials_name_says_which_link_it_was_issued_for() {
        let node = [7_u8; NODE_ID_LEN];
        let peer = names(node, Purpose::Peer);
        let client = names(node, Purpose::Client);
        assert_ne!(peer, client);
        assert!(peer.ends_with(".peer.tessari"), "{peer}");
        assert!(client.ends_with(".client.tessari"), "{client}");
        // The same node, so the difference is the purpose and nothing else.
        assert_eq!(
            peer.trim_end_matches(".peer.tessari"),
            client.trim_end_matches(".client.tessari")
        );
    }

    #[test]
    fn a_connection_that_presented_nothing_is_refused_before_anything_else() {
        let refused = presented(None, [1_u8; NODE_ID_LEN]).expect_err("nothing was presented");
        assert!(matches!(refused, Error::Unidentified), "{refused}");
    }

    #[test]
    fn a_fingerprint_is_a_sha256_in_lowercase_hex() {
        let der = CertificateDer::from(vec![0_u8, 1, 2, 3]);
        let printed = fingerprint(&der);
        assert_eq!(printed.len(), 64, "{printed}");
        assert!(
            printed
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "{printed}"
        );
    }

    #[test]
    fn a_certificate_that_is_not_one_answers_no_rather_than_panicking() {
        let der = CertificateDer::from(vec![0_u8; 8]);
        assert!(!valid_for(&der, "a.peer.tessari"));
    }
}
