//! What can go wrong reading or writing the Kafka protocol.
//!
//! Every variant is a client's mistake or a client's choice, never the store's:
//! the store's own refusals travel as Kafka error codes inside a well-formed
//! answer, and only a frame that cannot be read at all ends the connection.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A frame this node cannot read, or chooses not to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The frame ended before the field it was reading.
    #[error("the frame ends inside {field}")]
    Truncated {
        /// The field being read.
        field: &'static str,
    },

    /// A field held a value its encoding cannot carry.
    #[error("{field} is malformed: {why}")]
    Malformed {
        /// The field.
        field: &'static str,
        /// What was wrong with it.
        why: &'static str,
    },

    /// A length, a count or an expansion went past this node's ceiling.
    ///
    /// Refused rather than allocated: a length is a number a client chose, and
    /// a client that says four gigabytes must not get four gigabytes reserved.
    #[error("{field} is {found}, above the ceiling of {ceiling}")]
    TooLarge {
        /// The field.
        field: &'static str,
        /// What the client said.
        found: u64,
        /// The most this node accepts.
        ceiling: u64,
    },

    /// A record batch's checksum does not match its bytes.
    #[error("the record batch's checksum is {stored:#010x} and its bytes give {computed:#010x}")]
    Checksum {
        /// What the batch says.
        stored: u32,
        /// What its bytes give.
        computed: u32,
    },

    /// A record batch is compressed with a codec this node does not read.
    #[error("compression codec {codec} is not one of none, gzip, snappy, lz4, zstd")]
    Codec {
        /// The attribute bits.
        codec: u16,
    },

    /// A compressed record batch did not decompress.
    #[error("the {codec} payload does not decompress: {why}")]
    Decompress {
        /// The codec.
        codec: &'static str,
        /// What the decoder said.
        why: String,
    },

    /// The request names an API or a version this node does not answer.
    #[error("api {key} version {version} is not served here")]
    Unsupported {
        /// The API key.
        key: i16,
        /// The version asked for.
        version: i16,
    },
}
