//! Whether the client surfaces speak TLS, decided once at start-up (ADR-0108 D4).
//!
//! A cluster serves clients in the clear only when somebody said so: a node
//! with peer credentials has an operator who issued certificates already, and
//! one that came up in plaintext because a variable was misspelled looks
//! exactly like one that came up correctly. A single node keeps plaintext as
//! its default in this release and says so on every start (Q-883).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tessari_serve::tls::{self, Pem};

/// The certificate chain the client surfaces present, PEM.
pub(crate) const TLS_CERT: &str = "TESSARIDB_TLS_CERT";
/// Its private key, PEM.
pub(crate) const TLS_KEY: &str = "TESSARIDB_TLS_KEY";
/// `1` to serve clients in the clear on a cluster, which refuses otherwise.
pub(crate) const CLIENT_PLAINTEXT: &str = "TESSARIDB_CLIENT_PLAINTEXT";
/// The certificates `--at` trusts a node by, PEM.
pub(crate) const TLS_AUTHORITY: &str = "TESSARIDB_TLS_AUTHORITY";

/// What the flags and the environment said, before it is judged.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Given {
    pub(crate) cert: Option<PathBuf>,
    pub(crate) key: Option<PathBuf>,
    pub(crate) plaintext: bool,
}

impl Given {
    /// The flags, with the environment filling what they left out.
    ///
    /// Per field rather than all-or-nothing, so a container that sets the two
    /// paths in its environment can still be told `--client-plaintext` — which
    /// the judgement then refuses as two answers, rather than one of them being
    /// silently dropped here.
    pub(crate) fn with_environment(
        self,
        read: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let plaintext = match read(CLIENT_PLAINTEXT).as_deref() {
            None | Some("" | "0") => false,
            Some("1") => true,
            Some(other) => {
                return Err(format!("{CLIENT_PLAINTEXT} is 1 or 0, not {other:?}"));
            }
        };
        Ok(Self {
            cert: self.cert.or_else(|| read(TLS_CERT).map(PathBuf::from)),
            key: self.key.or_else(|| read(TLS_KEY).map(PathBuf::from)),
            plaintext: self.plaintext || plaintext,
        })
    }
}

/// How the client surfaces are served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Clients {
    /// TLS with this chain and key.
    Tls { cert: PathBuf, key: PathBuf },
    /// In the clear; `chosen` when somebody said so rather than by default.
    Plaintext { chosen: bool },
}

/// Judge what was given, for a node that is `clustered` or not.
///
/// # Errors
///
/// Half a certificate, a certificate beside `--client-plaintext`, and a
/// cluster told neither are refused rather than started.
pub(crate) fn decide(given: Given, clustered: bool) -> Result<Clients, String> {
    match (given.cert, given.key, given.plaintext) {
        (Some(_), Some(_), true) => Err(
            "--tls-cert and --client-plaintext are two answers to one question; give one"
                .to_owned(),
        ),
        (Some(cert), Some(key), false) => Ok(Clients::Tls { cert, key }),
        (Some(_), None, _) => Err("--tls-cert was given without --tls-key".to_owned()),
        (None, Some(_), _) => Err("--tls-key was given without --tls-cert".to_owned()),
        (None, None, true) => Ok(Clients::Plaintext { chosen: true }),
        (None, None, false) if clustered => Err(
            "a cluster node serves its clients over TLS: give --tls-cert and --tls-key, or \
             --client-plaintext to serve them in the clear on a network you trust"
                .to_owned(),
        ),
        (None, None, false) => Ok(Clients::Plaintext { chosen: false }),
    }
}

/// The server settings for the wire surface and the HTTP surface.
///
/// Two, because HTTP offers `http/1.1` by ALPN and the wire protocol offers
/// nothing a client would recognise.
///
/// # Errors
///
/// A file that cannot be read, or a certificate and key that cannot be used,
/// naming which.
pub(crate) fn settings(
    cert: &Path,
    key: &Path,
) -> Result<(Arc<rustls::ServerConfig>, Arc<rustls::ServerConfig>), String> {
    let chain = read(cert, "certificate")?;
    let private = read(key, "key")?;
    let (chain_at, key_at) = (cert.display().to_string(), key.display().to_string());
    let made = |alpn: &[&[u8]]| {
        tls::server_config(
            Pem {
                bytes: &chain,
                path: &chain_at,
            },
            Pem {
                bytes: &private,
                path: &key_at,
            },
            alpn,
        )
        .map_err(|refused| refused.to_string())
    };
    Ok((made(&[])?, made(&[b"http/1.1"])?))
}

/// The certificates `--at` trusts a node by.
///
/// # Errors
///
/// A file that cannot be read or holds no usable certificate.
pub(crate) fn authority(file: &Path) -> Result<rustls::RootCertStore, String> {
    let bytes = read(file, "authority")?;
    tls::authority(Pem {
        bytes: &bytes,
        path: &file.display().to_string(),
    })
    .map_err(|refused| refused.to_string())
}

fn read(file: &Path, part: &str) -> Result<Vec<u8>, String> {
    std::fs::read(file).map_err(|why| {
        format!(
            "the TLS {part} at {} could not be read: {why}",
            file.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CLIENT_PLAINTEXT, Clients, Given, TLS_CERT, TLS_KEY, decide};

    fn given(cert: Option<&str>, key: Option<&str>, plaintext: bool) -> Given {
        Given {
            cert: cert.map(PathBuf::from),
            key: key.map(PathBuf::from),
            plaintext,
        }
    }

    #[test]
    fn a_cluster_refuses_to_serve_clients_in_the_clear_unless_told_to() {
        let refused = decide(given(None, None, false), true).expect_err("a cluster in the clear");
        assert!(refused.contains("--client-plaintext"), "{refused}");
        assert_eq!(
            decide(given(None, None, true), true),
            Ok(Clients::Plaintext { chosen: true })
        );
        assert_eq!(
            decide(given(Some("c.pem"), Some("k.pem"), false), true),
            Ok(Clients::Tls {
                cert: PathBuf::from("c.pem"),
                key: PathBuf::from("k.pem"),
            })
        );
    }

    #[test]
    fn a_single_node_keeps_the_clear_by_default_and_says_it_was_not_chosen() {
        assert_eq!(
            decide(given(None, None, false), false),
            Ok(Clients::Plaintext { chosen: false })
        );
    }

    #[test]
    fn half_a_certificate_and_two_answers_are_refused() {
        for (asked, named) in [
            (given(Some("c.pem"), None, false), "without --tls-key"),
            (given(None, Some("k.pem"), false), "without --tls-cert"),
            (given(Some("c.pem"), Some("k.pem"), true), "two answers"),
        ] {
            let refused = decide(asked, false).expect_err(named);
            assert!(refused.contains(named), "{refused}");
        }
    }

    #[test]
    fn the_environment_fills_what_the_flags_left_out_and_never_overrides_them() {
        let environment = |name: &str| match name {
            TLS_CERT => Some("env-cert.pem".to_owned()),
            TLS_KEY => Some("env-key.pem".to_owned()),
            _ => None,
        };
        let merged = given(Some("flag-cert.pem"), None, false)
            .with_environment(environment)
            .expect("a readable environment");
        assert_eq!(
            merged,
            given(Some("flag-cert.pem"), Some("env-key.pem"), false)
        );

        let plaintext = |name: &str| (name == CLIENT_PLAINTEXT).then(|| "1".to_owned());
        assert!(
            Given::default()
                .with_environment(plaintext)
                .expect("1")
                .plaintext
        );
        let misspelled = |name: &str| (name == CLIENT_PLAINTEXT).then(|| "yes".to_owned());
        assert!(Given::default().with_environment(misspelled).is_err());
    }
}
