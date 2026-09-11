//! What can go wrong between a client and a node.
//!
//! Every variant answers a different question for whoever is looking at it: is
//! this the wrong program on the other end, the wrong version of the right one,
//! a client sending nonsense, or the store saying no? A single "protocol error"
//! would send somebody to read a packet capture for each of them.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure on the wire.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The socket failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A stored value could not be read back.
    #[error(transparent)]
    Encoding(#[from] tessari_encoding::Error),

    /// Whatever answered is not one of these.
    #[error("that is not a TessariDB node")]
    NotThisProtocol,

    /// It is one, of a version this build does not speak.
    ///
    /// Refused at the greeting rather than discovered mid-conversation, so a
    /// mismatch is one clear message at the start instead of a decode failure
    /// somewhere in the middle that reads like corruption.
    #[error("that node speaks version {found}; this build speaks {supported}")]
    WrongVersion {
        /// What it said.
        found: u8,
        /// What this build speaks.
        supported: u8,
    },

    /// A frame this build knows, arriving where a different one belongs.
    ///
    /// Kept apart from [`Error::UnknownFrame`] because the remedies are
    /// opposite: an unknown frame is a peer speaking a protocol this build does
    /// not have, while this one is a peer speaking the same protocol in the
    /// wrong order — a bug on the other side rather than a version to upgrade.
    #[error("frame kind {tag} arrived where the peer link expected a different one")]
    OutOfTurn {
        /// The tag that arrived.
        tag: u8,
    },

    /// A frame kind this build does not have.
    ///
    /// The connection closes rather than the frame being skipped: a protocol
    /// that ignores what it does not understand is one where a version mismatch
    /// looks like silence.
    #[error("frame kind {tag} is not one this build knows")]
    UnknownFrame {
        /// The tag that arrived.
        tag: u8,
    },

    /// A declared length above what this build will read.
    ///
    /// Refused **before** the allocation, because a length from a stranger is
    /// not a promise — and a server that allocated whatever a client declared
    /// would be one packet away from being out of memory.
    #[error("a frame declared {length} bytes, which is more than this build will read")]
    TooLarge {
        /// What it declared.
        length: u32,
    },

    /// The stream ended inside something.
    #[error("the connection ended mid-frame")]
    Truncated,

    /// A body that does not hold what it claims.
    #[error("a frame's body is not the shape its own header says")]
    Malformed,

    /// This node may not take the write, and knows of no peer that may.
    ///
    /// The forward's target is missing rather than unreachable: nothing is
    /// declared `writable`, so there is no address to try. Said plainly and
    /// separately from a failed dial, because the two have different remedies —
    /// one is a `DEFINE REPLICA … ROLES writable` nobody ran, the other is a
    /// peer that is down.
    #[error("this node does not accept writes, and no peer is declared writable")]
    NoWritablePeer,

    /// The store said no, and this is what it said.
    ///
    /// Carried through verbatim rather than reworded: the session already writes
    /// messages that name the place in the script, and a wire layer improving on
    /// them would be a second author for one error.
    #[error("{message}")]
    Refused {
        /// The store's own words.
        message: String,
    },

    /// A peer connection arrived having proved nothing at all.
    ///
    /// Refused rather than defaulted: a handshake that admitted an unproven
    /// peer would make every check after it a formality performed on whoever
    /// asked. On a link that carries the whole log of every tenant, absence of
    /// proof is the one answer that must never be treated as an omission.
    #[error("that connection proved no identity, and this link admits no strangers")]
    Unidentified,

    /// A credential that was never issued for peering.
    ///
    /// Separate from a disagreeing id because the id may be perfectly correct:
    /// what went wrong is that a credential meant for one link was presented on
    /// another, which is a mis-issue or a copied file rather than an imposture.
    #[error("that credential was not issued for the peer link")]
    NotAPeerCredential,

    /// The id in the frame is not the id the credential names.
    ///
    /// Both are named, because whoever reads this needs to know which of the
    /// two is the node they configured. A node that could claim any id in a
    /// frame would make the credential decorative.
    #[error("that node's greeting claims {said} while its credential names {presented}")]
    IdentityDisagrees {
        /// What the frame said.
        said: String,
        /// What the transport proved.
        presented: String,
    },

    /// The transport itself refused, and this is what it said.
    ///
    /// A handshake that fails has already decided the connection is not
    /// happening, and the reasons are the transport's vocabulary rather than
    /// this protocol's — an untrusted issuer, an expired certificate, a name the
    /// server does not carry. Carried through as its own words for the same
    /// reason [`Self::Refused`] is: rewording somebody else's diagnosis gives an
    /// operator two accounts of one event.
    #[error("the peer link's transport refused this connection: {0}")]
    Transport(String),

    /// A credential the transport accepted, which does not name this node.
    ///
    /// Distinct from [`Self::IdentityDisagrees`] by what it is able to say.
    /// That one is raised where the transport hands over an id it extracted, so
    /// both ids can be named. Here the credential was *asked* whether it speaks
    /// for the id the greeting claims and said no, and asking cannot report
    /// which id it would have said yes to. The fingerprint is the better half of
    /// that answer regardless: it names exactly one file on exactly one machine,
    /// which is what an operator chasing a mis-issued credential has to find.
    #[error(
        "that node's greeting claims {said}, and the credential it presented (sha256 {fingerprint}) does not name it"
    )]
    CredentialNamesAnother {
        /// What the frame said.
        said: String,
        /// The SHA-256 of the certificate that was presented, lowercase hex.
        fingerprint: String,
    },

    /// A role set carrying a bit this build does not assign.
    ///
    /// Not a malformed frame — the body is exactly the shape a greeting takes.
    /// It carries a fact from a build that knows more than this one, and
    /// reporting that as corruption would send somebody to the wrong question.
    #[error("that node holds role bits {bits:#010b}, which this build does not know")]
    UnknownRoles {
        /// The bits that arrived.
        bits: u8,
    },
}
