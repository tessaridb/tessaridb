//! Why a certificate, a key or an authority is refused.

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
