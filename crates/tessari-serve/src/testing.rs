//! A TLS pair for a test: a node's settings and a client's that trusts it,
//! minted per call for `127.0.0.1` so no key is ever kept (G079 C8).

use std::sync::Arc;

use rustls::{ClientConfig, ServerConfig};

use crate::tls::{Credential, Pem, authority};

/// Server settings for `127.0.0.1` and client settings trusting the
/// authority that issued them.
///
/// # Errors
///
/// What minting or reading the certificate refused, in its own words.
pub fn loopback() -> Result<(Arc<ServerConfig>, Arc<ClientConfig>), String> {
    let authority_key = rcgen::KeyPair::generate().map_err(|why| why.to_string())?;
    let mut asked =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|why| why.to_string())?;
    asked.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer = asked
        .self_signed(&authority_key)
        .map_err(|why| why.to_string())?;
    let leaf_key = rcgen::KeyPair::generate().map_err(|why| why.to_string())?;
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .map_err(|why| why.to_string())?
        .signed_by(&leaf_key, &issuer, &authority_key)
        .map_err(|why| why.to_string())?;
    let (chain, key, trusted) = (leaf.pem(), leaf_key.serialize_pem(), issuer.pem());
    let server = Credential::read(
        Pem {
            bytes: chain.as_bytes(),
            path: "cert.pem",
        },
        Pem {
            bytes: key.as_bytes(),
            path: "key.pem",
        },
    )
    .map_err(|why| why.to_string())?
    .server_config(&[]);
    let roots = authority(Pem {
        bytes: trusted.as_bytes(),
        path: "ca.pem",
    })
    .map_err(|why| why.to_string())?;
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((server, Arc::new(client)))
}
