//! The certificates this process presents, for `INFO FOR NODE` (ADR-0108 D9).
//!
//! Read through the same handles the doors present from, at the moment of the
//! report, so a renewal the credentials watcher swapped in is what the report
//! names — the same rule the expiry gauges follow.

/// The client surface's credential and the peer link's keys, when each exists.
#[derive(Debug)]
pub(crate) struct Shown {
    pub(crate) clients: Option<tessari_serve::tls::Credential>,
    pub(crate) peers: Option<tessari_wire::PeerKeys>,
    /// `--require-client-tls` was in force at start-up.
    pub(crate) required: bool,
}

impl tessari_session::Certificates for Shown {
    fn presented(&self) -> Vec<tessari_session::Presented> {
        let described = |surface, leaf: Option<rustls::pki_types::CertificateDer<'static>>| {
            leaf.map(|leaf| tessari_session::Presented {
                surface,
                fingerprint: tessari_wire::fingerprint(&leaf),
                expires: tessari_serve::tls::not_after(&leaf),
            })
        };
        [
            described(
                "peers",
                self.peers.as_ref().and_then(tessari_wire::PeerKeys::leaf),
            ),
            described(
                "clients",
                self.clients
                    .as_ref()
                    .and_then(tessari_serve::tls::Credential::leaf),
            ),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn clients(&self) -> tessari_session::ClientTransport {
        tessari_session::ClientTransport {
            tls: self.clients.is_some(),
            required: self.required,
        }
    }
}
