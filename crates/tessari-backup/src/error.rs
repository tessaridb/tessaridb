//! What a backup or a restore can refuse.

/// What went wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The stream could not be read or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The store refused the work.
    #[error(transparent)]
    Store(#[from] tessari_storage::Error),

    /// A record could not be decoded.
    #[error(transparent)]
    Encoding(#[from] tessari_encoding::Error),

    /// One sequence was named where several logs are in play.
    ///
    /// A whole backup and a whole restore span every log a store holds. The two
    /// surfaces bounded by a single sequence — an incremental backup `FROM n`
    /// and a point-in-time restore `UPTO n` — cannot: sequence 500 in one log
    /// and sequence 500 in another are unrelated moments, and a number that
    /// silently meant the first of them would produce a store no log explains.
    /// Refused rather than guessed at until both surfaces name a position per
    /// log (Q-624, Q-621).
    #[error(
        "{what} names one sequence and {logs} logs are in play; \
         a sequence counts in one log alone"
    )]
    ManyLogs {
        /// Which surface named the sequence.
        what: &'static str,
        /// How many logs are in play.
        logs: usize,
    },

    /// The file does not begin the way one of these does.
    #[error("this is not a TessariDB backup")]
    NotABackup,

    /// The file was written by a build newer than this one.
    ///
    /// Refused, and deliberately **not** symmetric with an older file: an older
    /// backup restoring into a newer build is the ordinary case and the whole
    /// reason the version is recorded. The other direction is not, for the
    /// reason a newer on-disk format is refused — a newer writer may have given
    /// a record a meaning this build does not know, and the framing bytes cannot
    /// see that, because a newer build writes byte-identical framing.
    #[error(
        "this backup was written by version {found}; this build is {supported} \
         and will not guess at what a newer one meant"
    )]
    WrittenByNewer {
        /// The version that wrote the file.
        found: String,
        /// The version reading it.
        supported: String,
    },

    /// A format or codec version this build does not read.
    ///
    /// Refused rather than attempted, because a decoder that guesses at a
    /// version it does not know produces records nobody wrote.
    #[error("this backup is {what} version {found}; this build reads {supported}")]
    Unsupported {
        /// Which version — the framing's or the records'.
        what: &'static str,
        /// The version the file carries.
        found: u8,
        /// The version this build reads.
        supported: u8,
    },

    /// A restore into a store that is not where this file continues from.
    ///
    /// A whole backup starts at sequence 1 and needs an empty store; an
    /// incremental one starts at `from` and needs a store standing at
    /// `from - 1`. Anything else is not a restore: the sequences would land with
    /// a different meaning and the result would be a store no log explains.
    #[error("this backup continues from sequence {needs}; the store is at {found}")]
    WrongBase {
        /// Where the store would have to be.
        needs: u64,
        /// Where it actually is.
        found: u64,
    },

    /// A record whose bytes are not the bytes that were written.
    ///
    /// The check that framing cannot do. Raised rather than reported, because a
    /// record that decodes into something nobody wrote is worse than a restore
    /// that stops — and a caller who wants what is whole can verify first and
    /// restore to the last good sequence.
    #[error("the record at sequence {sequence} is damaged")]
    Damaged {
        /// Where in the log it was.
        sequence: u64,
    },

    /// A snapshot that ends before its end frame, or whose end frame does not
    /// count what was read.
    ///
    /// Refused whole rather than restored in part: a cut log is a prefix of
    /// history, which is a state the store once held, and a cut snapshot is an
    /// arbitrary subset of records, which is not (ADR-0091 §5).
    #[error(
        "this snapshot is not whole — it ends before its end frame or holds \
         fewer records than it says; a snapshot is restored whole or not at all"
    )]
    StateIncomplete,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
