//! Whether the client surfaces speak TLS, decided once at start-up (ADR-0111).
//!
//! Client TLS is opt-in on every node, single or clustered: a certificate turns
//! it on, and without one the node serves its clients in the clear and says so
//! on every start. A deployment whose policy forbids the clear says
//! `--require-client-tls`, and the node then refuses to start without a
//! certificate rather than serve one connection unencrypted. The peer link is
//! not decided here: it is always mutual TLS (ADR-0062).

use std::path::{Path, PathBuf};

use tessari_serve::tls::{self, Pem};

/// The certificate chain the client surfaces present, PEM.
pub(crate) const TLS_CERT: &str = "TESSARIDB_TLS_CERT";
/// Its private key, PEM.
pub(crate) const TLS_KEY: &str = "TESSARIDB_TLS_KEY";
/// Retired in 0.24.0-beta: `1` used to serve clients in the clear as a
/// choice, and the clear has been the default without a certificate since
/// ADR-0111, so a node told it now refuses to start.
pub(crate) const CLIENT_PLAINTEXT: &str = "TESSARIDB_CLIENT_PLAINTEXT";

/// Why `--client-plaintext` or its variable is refused.
pub(crate) const PLAINTEXT_RETIRED: &str = "was retired in 0.24.0-beta: a node serves its \
     clients in the clear by default when it has no certificate, so drop it";
/// `1` to refuse to start without a client certificate.
pub(crate) const REQUIRE_CLIENT_TLS: &str = "TESSARIDB_REQUIRE_CLIENT_TLS";
/// The certificates `--at` trusts a node by, PEM.
pub(crate) const TLS_AUTHORITY: &str = "TESSARIDB_TLS_AUTHORITY";

/// What the flags and the environment said, before it is judged.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Given {
    pub(crate) cert: Option<PathBuf>,
    pub(crate) key: Option<PathBuf>,
    pub(crate) require: bool,
}

impl Given {
    /// The flags, with the environment filling what they left out.
    ///
    /// Per field rather than all-or-nothing, so a container that sets one
    /// path in its environment and the other by flag gets both.
    ///
    /// # Errors
    ///
    /// A switch that is not `1` or `0`, and the retired plaintext variable set.
    pub(crate) fn with_environment(
        self,
        read: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        if switch(&read, CLIENT_PLAINTEXT)? {
            return Err(format!("{CLIENT_PLAINTEXT}=1 {PLAINTEXT_RETIRED}"));
        }
        let require = switch(&read, REQUIRE_CLIENT_TLS)?;
        Ok(Self {
            cert: self.cert.or_else(|| read(TLS_CERT).map(PathBuf::from)),
            key: self.key.or_else(|| read(TLS_KEY).map(PathBuf::from)),
            require: self.require || require,
        })
    }
}

/// A `1`/`0` variable, absent or empty meaning `0`; anything else is refused
/// naming the variable and the value, because a misspelt `yes` that read as off
/// would leave a policy unenforced with nothing in an error state.
fn switch(read: &impl Fn(&str) -> Option<String>, name: &str) -> Result<bool, String> {
    match read(name).as_deref() {
        None | Some("" | "0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(format!("{name} is 1 or 0, not {other:?}")),
    }
}

/// How the client surfaces are served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Clients {
    /// TLS with this chain and key; `required` when the node was told it may
    /// serve no other way.
    Tls {
        cert: PathBuf,
        key: PathBuf,
        required: bool,
    },
    /// In the clear, which is what a node with no certificate does.
    Plaintext,
}

/// Judge what was given.
///
/// # Errors
///
/// Half a certificate, and the switch with no certificate, are refused rather
/// than started.
pub(crate) fn decide(given: Given) -> Result<Clients, String> {
    let Given { cert, key, require } = given;
    match (cert, key) {
        (Some(cert), Some(key)) => Ok(Clients::Tls {
            cert,
            key,
            required: require,
        }),
        (Some(_), None) => Err("--tls-cert was given without --tls-key".to_owned()),
        (None, Some(_)) => Err("--tls-key was given without --tls-cert".to_owned()),
        (None, None) if require => Err(format!(
            "--require-client-tls ({REQUIRE_CLIENT_TLS}=1) serves clients over TLS only: give \
             --tls-cert and --tls-key ({TLS_CERT} and {TLS_KEY})"
        )),
        (None, None) => Ok(Clients::Plaintext),
    }
}

/// Whether a password typed into a client of these addresses can cross a
/// network, for the plaintext start line (ADR-0111 D3): an address that does not
/// read as loopback — `0.0.0.0` and a name included — is reachable beyond this
/// machine.
pub(crate) fn reach(addresses: &[String]) -> &'static str {
    let loopback = |address: &String| {
        address
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|bound| bound.ip().is_loopback())
    };
    if addresses.iter().all(loopback) {
        "on loopback only"
    } else {
        "reachable beyond this machine"
    }
}

/// The certificate both client surfaces present, read from its two files.
///
/// One credential for both, so one reload reaches both. Each surface builds its
/// own settings over it, because HTTP offers `http/1.1` by ALPN and the wire
/// protocol offers nothing a client would recognise.
///
/// # Errors
///
/// A file that cannot be read, or a certificate and key that cannot be used,
/// naming which.
pub(crate) fn credential(cert: &Path, key: &Path) -> Result<tls::Credential, String> {
    let chain = read(cert, "certificate")?;
    let private = read(key, "key")?;
    tls::Credential::read(
        Pem {
            bytes: &chain,
            path: &cert.display().to_string(),
        },
        Pem {
            bytes: &private,
            path: &key.display().to_string(),
        },
    )
    .map_err(|refused| refused.to_string())
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

    use super::{
        CLIENT_PLAINTEXT, Clients, Given, REQUIRE_CLIENT_TLS, TLS_CERT, TLS_KEY, decide, reach,
    };

    fn given(cert: Option<&str>, key: Option<&str>) -> Given {
        Given {
            cert: cert.map(PathBuf::from),
            key: key.map(PathBuf::from),
            require: false,
        }
    }

    fn requiring(cert: Option<&str>, key: Option<&str>) -> Given {
        Given {
            require: true,
            ..given(cert, key)
        }
    }

    #[test]
    fn a_node_told_nothing_serves_its_clients_in_the_clear() {
        // ADR-0111 D2: single node and cluster alike — the cluster refusal is gone.
        assert_eq!(decide(given(None, None)), Ok(Clients::Plaintext));
    }

    #[test]
    fn a_certificate_serves_tls_and_says_whether_it_was_required() {
        assert_eq!(
            decide(given(Some("c.pem"), Some("k.pem"))),
            Ok(Clients::Tls {
                cert: PathBuf::from("c.pem"),
                key: PathBuf::from("k.pem"),
                required: false,
            })
        );
        assert_eq!(
            decide(requiring(Some("c.pem"), Some("k.pem"))),
            Ok(Clients::Tls {
                cert: PathBuf::from("c.pem"),
                key: PathBuf::from("k.pem"),
                required: true,
            })
        );
    }

    #[test]
    fn a_node_required_to_serve_tls_refuses_to_start_without_a_certificate() {
        let refused = decide(requiring(None, None)).expect_err("required and absent");
        assert!(
            refused.contains("--tls-cert") && refused.contains("--require-client-tls"),
            "{refused}"
        );
    }

    #[test]
    fn the_retired_plaintext_variable_is_refused_and_its_zero_is_harmless() {
        let on = |name: &str| (name == CLIENT_PLAINTEXT).then(|| "1".to_owned());
        let refused = Given::default().with_environment(on).expect_err("retired");
        assert!(
            refused.contains(CLIENT_PLAINTEXT) && refused.contains("retired"),
            "{refused}"
        );
        let off = |name: &str| (name == CLIENT_PLAINTEXT).then(|| "0".to_owned());
        assert_eq!(Given::default().with_environment(off), Ok(Given::default()));
    }

    #[test]
    fn half_a_certificate_is_refused() {
        for (asked, named) in [
            (given(Some("c.pem"), None), "without --tls-key"),
            (given(None, Some("k.pem")), "without --tls-cert"),
            (requiring(Some("c.pem"), None), "without --tls-key"),
        ] {
            let refused = decide(asked).expect_err(named);
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
        let merged = given(Some("flag-cert.pem"), None)
            .with_environment(environment)
            .expect("a readable environment");
        assert_eq!(merged, given(Some("flag-cert.pem"), Some("env-key.pem")));

        let variable = REQUIRE_CLIENT_TLS;
        let on = |name: &str| (name == variable).then(|| "1".to_owned());
        assert!(Given::default().with_environment(on).expect("1").require);
        let off = |name: &str| (name == variable).then(|| "0".to_owned());
        assert!(!Given::default().with_environment(off).expect("0").require);
        let misspelled = |name: &str| (name == variable).then(|| "yes".to_owned());
        let refused = Given::default()
            .with_environment(misspelled)
            .expect_err("yes is not 1 or 0");
        assert!(
            refused.contains(variable) && refused.contains("yes"),
            "{refused}"
        );
    }

    #[test]
    fn the_switch_is_off_unless_somebody_turns_it_on() {
        let nothing = |_: &str| None;
        assert!(
            !Given::default()
                .with_environment(nothing)
                .expect("empty")
                .require
        );
    }

    #[test]
    fn an_address_that_is_not_loopback_is_reachable_beyond_this_machine() {
        let owned = |addresses: &[&str]| {
            addresses
                .iter()
                .map(|a| (*a).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            reach(&owned(&["127.0.0.1:9080", "[::1]:8000"])),
            "on loopback only"
        );
        for beyond in [
            owned(&["0.0.0.0:9080"]),
            owned(&["127.0.0.1:9080", "10.0.0.5:8000"]),
            owned(&["db:7654"]),
        ] {
            assert_eq!(
                reach(&beyond),
                "reachable beyond this machine",
                "{beyond:?}"
            );
        }
    }
}
