//! What a backup, a check and a restore report back.

use super::*;

/// One log, and the range of it a file's section holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSpan {
    /// The log the section holds.
    pub log: LogId,
    /// The first sequence of it the section holds.
    pub from: Sequence,
    /// The sequence that log was at when the section was taken.
    pub tail: Sequence,
}

/// What a backup wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// How many log records it holds, across every section.
    pub records: u64,
    /// What each section covers, in the order they were written.
    ///
    /// A list rather than one pair because a store holds a log per range, and
    /// reporting the last section's bounds as the file's would be a number that
    /// is right about a part and wrong about the whole.
    pub logs: Vec<LogSpan>,
    /// The build that wrote it.
    pub writer: NodeVersion,
}

/// One section of a file, as reading it without applying it found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedLog {
    /// What the section says it covers.
    pub span: LogSpan,
    /// The last sequence in it that read whole and checked out.
    ///
    /// What a restore of this log could safely be stopped at, which is the
    /// number somebody holding a damaged file actually needs.
    pub good_through: Sequence,
}

/// What a backup turned out to hold, without any of it being applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The build that wrote the file.
    pub written_by: NodeVersion,
    /// How many records read whole and checked out, across every section.
    pub records: u64,
    /// What each section says it holds, and how far it actually reads.
    pub logs: Vec<VerifiedLog>,
    /// Whether the file ended mid-record, or before every section arrived.
    pub truncated: bool,
}

/// What a restore applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The build that wrote the file.
    ///
    /// Reported rather than checked against anything beyond "not newer than
    /// this one": restoring an older backup into a newer build is the case this
    /// field exists to make visible, not one to refuse.
    pub written_by: NodeVersion,
    /// How many records were applied, across every section.
    pub records: u64,
    /// What each section the restore reached said it covered.
    pub logs: Vec<LogSpan>,
    /// Whether the file ended mid-record, or before every section arrived.
    ///
    /// Reported rather than raised: an interrupted backup is still most of a
    /// store, and the caller is the one who knows whether most is enough.
    pub truncated: bool,
}

/// What a bootstrap left the node holding, and where it must continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrapped {
    /// The build that wrote the prefix.
    pub written_by: NodeVersion,
    /// How many log records were applied.
    pub records: u64,
    /// The sequence to ask the leader for next, per log the node now holds.
    ///
    /// Taken from the node's **own** committed tail after the replay, never from
    /// what the prefix said it held — see [`bootstrap`]. One entry per log,
    /// because a node holding several has several positions and a single number
    /// would be right about one of them (Q-620, Q-621).
    pub follow_from: Vec<(LogId, Sequence)>,
    /// Whether the prefix ended mid-record.
    ///
    /// A truncated prefix leaves a node that is a correct copy of an *earlier*
    /// moment, not a broken one: `follow_from` is where it actually reached, so
    /// following resumes with nothing missed. Reported because the node is
    /// further behind than whoever sent the prefix intended.
    pub truncated: bool,
}
