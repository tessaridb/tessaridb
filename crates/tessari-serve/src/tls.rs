//! TLS on the client surfaces (ADR-0108 D4).
//!
//! Both surfaces take the same certificate and key, and a refusal must name the
//! same part of the same file whichever surface read it, so the reading lives
//! here rather than once per surface. What each surface does with the result —
//! the wire node wraps a socket, HTTP wraps a listener — stays with it.
//!
//! TLS 1.3 only, with rustls's own cipher suites: these are external-facing
//! surfaces, every client this project ships speaks 1.3, and there is no
//! setting that widens either, because a legacy version enabled for one old
//! client is offered to every client, including one that downgrades on purpose.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

pub use crate::error::Refused;
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{RootCertStore, ServerConfig};

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

/// The certificate the client surfaces present, replaceable while they serve
/// (ADR-0108 D6).
///
/// Cloning shares it. Both surfaces' settings resolve the certificate from
/// here at each handshake, so one [`Self::replace`] reaches the wire and HTTP
/// alike from their next connection on, and an open connection finishes on
/// the certificate it started with. The lock is held only to clone an `Arc`.
#[derive(Debug, Clone)]
pub struct Credential {
    current: Arc<Mutex<Arc<CertifiedKey>>>,
}

impl Credential {
    /// This node's certificate chain (leaf first) and its private key.
    ///
    /// The key is checked against the leaf here, so a pair that does not
    /// belong together is refused when it is read rather than at every
    /// client's handshake.
    ///
    /// # Errors
    ///
    /// [`Refused`] naming the part that could not be used.
    pub fn read(chain: Pem<'_>, key: Pem<'_>) -> Result<Self, Refused> {
        Ok(Self {
            current: Arc::new(Mutex::new(certified(chain, key)?)),
        })
    }

    /// Present `chain` and `key` from the next handshake on.
    ///
    /// Checked whole before anything changes, so a refused replacement leaves
    /// the surfaces presenting what they presented before.
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn replace(&self, chain: Pem<'_>, key: Pem<'_>) -> Result<(), Refused> {
        let next = certified(chain, key)?;
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = next;
        Ok(())
    }

    /// The leaf presented now.
    #[must_use]
    pub fn leaf(&self) -> Option<CertificateDer<'static>> {
        self.held().cert.first().cloned()
    }

    /// The server side of a client surface, offering `alpn` in order of
    /// preference and presenting whatever this credential holds.
    #[must_use]
    pub fn server_config(&self, alpn: &[&[u8]]) -> Arc<ServerConfig> {
        let mut settings = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(self.clone()));
        settings.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
        Arc::new(settings)
    }

    fn held(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl ResolvesServerCert for Credential {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.held())
    }
}

/// A chain and a key, read and checked against each other.
fn certified(chain: Pem<'_>, key: Pem<'_>) -> Result<Arc<CertifiedKey>, Refused> {
    let certificates = chain.certificates("certificate")?;
    let private = PrivateKeyDer::from_pem_slice(key.bytes).map_err(|why| match why {
        pem::Error::NoItemsFound => Refused::Empty {
            part: "key",
            path: key.path.to_owned(),
            wanted: "private key",
        },
        why => key.unreadable("key", &why),
    })?;
    let mismatched = |why: rustls::Error| Refused::Mismatched {
        reason: why.to_string(),
    };
    let signing = rustls::crypto::ring::sign::any_supported_type(&private).map_err(mismatched)?;
    let certified = CertifiedKey::new(certificates, signing);
    certified.keys_match().map_err(mismatched)?;
    Ok(Arc::new(certified))
}

/// A private key file's bytes, refused when anybody but its owner may read it.
///
/// The rule a private SSH key follows: a key that every account on the host can
/// read authenticates this node to nobody who can log in to it. Checked before
/// the bytes are read, so a refused key is never held in memory. Off Unix there
/// is no mode to check.
///
/// One allowance, PostgreSQL's: a file **owned by root** may also be readable by
/// its group. That is how an orchestrator mounts a secret for a process running
/// as another user — root owns it, the process's group may read it — and refusing
/// it would leave no way to hand this node a key there.
///
/// # Errors
///
/// The file cannot be read, or its group or others may read it — the message
/// names the mode and the fix.
pub fn read_private_key(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path).map_err(|why| why.to_string())?;
    owner_only(&metadata)?;
    std::fs::read(path).map_err(|why| why.to_string())
}

#[cfg(unix)]
fn owner_only(metadata: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let mode = metadata.permissions().mode();
    let others = mode & 0o007 != 0;
    let group = mode & 0o070 != 0 && metadata.uid() != 0;
    if others || group {
        Err(format!(
            "may be read by others (mode {:o}); `chmod 600` it",
            mode & 0o777
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn owner_only(_: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
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

/// When `certificate` stops being valid, in seconds since the Unix epoch.
///
/// Read for the expiry gauges (ADR-0108 D6): an expired certificate is a
/// refused handshake and therefore an outage, so its date is a number an
/// operator alerts on rather than one found in a log afterwards. A bounded walk
/// to `tbsCertificate.validity.notAfter` and nothing else — no certificate
/// parsing library is in the tree, and the date is the only field read.
///
/// `None` for anything that is not a certificate this walk can read; it never
/// reads past the bytes it was given.
#[must_use]
pub fn not_after(certificate: &CertificateDer<'_>) -> Option<i64> {
    const SEQUENCE: u8 = 0x30;
    let (whole, _) = element(certificate.as_ref(), SEQUENCE)?;
    let (signed, _) = element(whole, SEQUENCE)?;
    // The version is optional and explicitly tagged; the serial, the signature
    // algorithm and the issuer stand before the validity in every version.
    let rest = match element(signed, 0xa0) {
        Some((_, rest)) => rest,
        None => signed,
    };
    let (_, rest) = element(rest, 0x02)?;
    let (_, rest) = element(rest, SEQUENCE)?;
    let (_, rest) = element(rest, SEQUENCE)?;
    let (validity, _) = element(rest, SEQUENCE)?;
    let (_, after) = moment(validity)?;
    moment(after).map(|(seconds, _)| seconds)
}

/// One DER element tagged `tag`: its content, and what follows it.
fn element(bytes: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&found, rest) = bytes.split_first()?;
    if found != tag {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let width = usize::from(first & 0x7f);
        if width == 0 || width > 4 {
            return None;
        }
        let (digits, rest) = rest.split_at_checked(width)?;
        let length = digits.iter().try_fold(0_usize, |held, &digit| {
            held.checked_mul(256)?.checked_add(usize::from(digit))
        })?;
        (length, rest)
    };
    rest.split_at_checked(length)
}

/// A UTCTime or GeneralizedTime in Zulu, as Unix seconds, and what follows it.
fn moment(bytes: &[u8]) -> Option<(i64, &[u8])> {
    let (written, rest, century) = match element(bytes, 0x17) {
        Some((written, rest)) => (written, rest, None),
        None => {
            let (written, rest) = element(bytes, 0x18)?;
            (written, rest, Some(()))
        }
    };
    let digits = written.strip_suffix(b"Z")?;
    let number = |from: usize, width: usize| -> Option<i64> {
        digits
            .get(from..from.checked_add(width)?)?
            .iter()
            .try_fold(0_i64, |held, &digit| {
                digit.is_ascii_digit().then(|| {
                    held.checked_mul(10)?
                        .checked_add(i64::from(digit.checked_sub(b'0')?))
                })?
            })
    };
    // RFC 5280 §4.1.2.5.1: a two-digit year of 50 or more is 19xx.
    let (year, at): (i64, usize) = match century {
        Some(()) if digits.len() == 14 => (number(0, 4)?, 4),
        None if digits.len() == 12 => {
            let short = number(0, 2)?;
            (
                if short >= 50 { 1900_i64 } else { 2000 }.checked_add(short)?,
                2,
            )
        }
        _ => return None,
    };
    let field = |index: usize| number(at.checked_add(index.checked_mul(2)?)?, 2);
    let (month, day) = (field(0)?, field(1)?);
    let (hour, minute, second) = (field(2)?, field(3)?, field(4)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_since_epoch(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    Some((seconds, rest))
}

/// Days from 1970-01-01 to a civil date in the proleptic Gregorian calendar.
fn days_since_epoch(year: i64, month: i64, day: i64) -> Option<i64> {
    let year = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = year.div_euclid(400);
    let of_era = year.checked_sub(era.checked_mul(400)?)?;
    let from_march = (month.checked_add(9)?).rem_euclid(12);
    let of_year = from_march
        .checked_mul(153)?
        .checked_add(2)?
        .checked_div(5)?
        .checked_add(day)?
        .checked_sub(1)?;
    let of_era_days = of_era
        .checked_mul(365)?
        .checked_add(of_era.checked_div(4)?)?
        .checked_sub(of_era.checked_div(100)?)?
        .checked_add(of_year)?;
    era.checked_mul(146_097)?
        .checked_add(of_era_days)?
        .checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::{Credential, Pem, Refused, authority, not_after, read_private_key};

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

    #[cfg(unix)]
    #[test]
    fn a_private_key_others_on_the_host_may_read_is_refused_and_one_only_its_owner_reads_is_taken()
    {
        use std::os::unix::fs::PermissionsExt as _;

        let (_, key) = minted();
        let file = tempfile::NamedTempFile::new().expect("a file");
        std::fs::write(file.path(), &key).expect("written");

        for (mode, shown) in [(0o644, "644"), (0o640, "640"), (0o604, "604")] {
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(mode))
                .expect("chmod");
            let refused = read_private_key(file.path()).expect_err("readable by others");
            assert!(
                refused.contains(shown) && refused.contains("chmod 600"),
                "{refused}"
            );
        }

        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
        assert_eq!(
            read_private_key(file.path()).expect("private"),
            key.as_bytes()
        );
    }

    #[test]
    fn a_private_key_that_is_not_there_says_so() {
        let refused = read_private_key(std::path::Path::new("/nonexistent/key.pem"))
            .expect_err("no such file");
        assert!(!refused.is_empty());
    }

    #[test]
    fn a_certificate_and_its_key_make_a_server_and_offer_what_was_asked() {
        let (certificate, key) = minted();
        let settings = Credential::read(pem(&certificate, "cert.pem"), pem(&key, "key.pem"))
            .expect("a usable credential")
            .server_config(&[b"http/1.1"]);
        assert_eq!(settings.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn each_part_that_cannot_be_used_is_named_with_its_file() {
        let (certificate, key) = minted();
        let (_, someone_elses) = minted();

        let no_certificate = Credential::read(pem(&key, "cert.pem"), pem(&key, "key.pem"))
            .expect_err("a key is not a certificate");
        assert!(
            matches!(&no_certificate, Refused::Empty { part: "certificate", path, .. } if path == "cert.pem"),
            "{no_certificate}"
        );

        let no_key = Credential::read(pem(&certificate, "cert.pem"), pem(&certificate, "key.pem"))
            .expect_err("a certificate is not a key");
        assert!(
            matches!(&no_key, Refused::Empty { part: "key", path, .. } if path == "key.pem"),
            "{no_key}"
        );

        let mismatched = Credential::read(
            pem(&certificate, "cert.pem"),
            pem(&someone_elses, "key.pem"),
        )
        .expect_err("another certificate's key");
        assert!(
            matches!(mismatched, Refused::Mismatched { .. }),
            "{mismatched}"
        );
    }

    #[test]
    fn the_date_a_certificate_expires_is_read_in_both_time_forms() {
        // 2031 is written as a UTCTime and 2051 as a GeneralizedTime (RFC 5280
        // §4.1.2.5): both forms are what a real issuer writes.
        for ((year, month, day), expected) in [
            ((2031, 5, 17), 1_936_742_400),
            ((2051, 1, 2), 2_556_230_400),
        ] {
            let mut params =
                rcgen::CertificateParams::new(vec!["localhost".to_owned()]).expect("parameters");
            params.not_after = rcgen::date_time_ymd(year, month, day);
            let key = rcgen::KeyPair::generate().expect("a key");
            let made = params.self_signed(&key).expect("a certificate");
            assert_eq!(
                not_after(made.der()),
                Some(expected),
                "{year}-{month}-{day}"
            );
        }
        assert_eq!(not_after(&b"not a certificate"[..].into()), None);
    }

    #[test]
    fn a_replacement_is_presented_and_a_refused_one_changes_nothing() {
        let (certificate, key) = minted();
        let held = Credential::read(pem(&certificate, "cert.pem"), pem(&key, "key.pem"))
            .expect("a usable credential");
        let shared = held.clone();
        let first = held.leaf().expect("a leaf");

        let (renewed, renewed_key) = minted();
        shared
            .replace(pem(&renewed, "cert.pem"), pem(&renewed_key, "key.pem"))
            .expect("a usable replacement");
        let second = held.leaf().expect("a leaf");
        assert_ne!(first, second, "every clone sees the replacement");

        let refused = shared
            .replace(pem(&certificate, "cert.pem"), pem(&renewed_key, "key.pem"))
            .expect_err("a key that is not the leaf's");
        assert!(matches!(refused, Refused::Mismatched { .. }), "{refused}");
        assert_eq!(
            held.leaf(),
            Some(second),
            "the refused pair replaced nothing"
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
