use super::*;

/// Where the statements come from, or what else was asked for.
#[derive(Debug)]
pub enum Source {
    /// Standard input, prompting or not depending on what it is.
    Standard,
    /// One script given on the command line.
    Inline(String),
    /// A file.
    File(PathBuf),
    /// Write this store's log to a file.
    Backup(PathBuf),
    /// Write this store's current state to a file (ADR-0091).
    Snapshot(PathBuf),
    /// Write this store's current state as a TessariQL script (ADR-0091).
    Dump(PathBuf),
    /// Replay a file into this store.
    Restore(PathBuf),
    /// Say whether the store is well.
    Health,
    /// Read a backup and say what it holds, without applying any of it.
    ///
    /// Needs no store, which is the point: a backup that can only be checked by
    /// restoring it is a backup nobody checks.
    Verify(PathBuf),
    /// Serve this store, on whichever surfaces `Asked::serving` names.
    Serve,
    /// Say which build this is, and nothing else.
    ///
    /// Needs no store, like `Verify`, and for a stronger reason: the first
    /// thing anybody does with a binary they have just been handed is ask it
    /// what it is, and a version that could only be obtained by opening a store
    /// would be unavailable at exactly that moment.
    Version,
    /// Print the usage and stop.
    ///
    /// A request rather than a refusal, which is the whole reason it is a
    /// variant instead of an early `Err`: asking a program for its help is not
    /// an error, and answering on standard error with a non-zero status breaks
    /// `tessaridb --help | grep serve` and fails any packaging smoke test that
    /// runs it.
    Help,
}
