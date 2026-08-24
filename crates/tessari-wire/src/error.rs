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
}
