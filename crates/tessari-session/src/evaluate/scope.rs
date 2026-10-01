//! What an expression is evaluated against: the record, and what the read around it knows.

use tessari_ql::{BinaryOp, Expr};
use tessari_types::{Analyzer, Path, RecordId, Value};

use crate::noticed::Noticed;
use crate::outcome::Note;
use crate::search::{Ranked, Searched};

/// The two channels a read reports on, which travel together everywhere.
///
/// A note the source *decides* to raise — a fall-back, an approximate path, a
/// subquery that reached its ceiling — is pushed straight onto `collected`. A
/// note the *evaluator* discovers while comparing values is recorded in
/// `noticed` and drained when the read reports. One parameter rather than two,
/// because they were being added to the same signatures one at a time and were
/// drifting apart at the call sites.
#[derive(Debug)]
/// What a walk needs to test a condition against each record it finds.
///
/// The three travel together because they are one question asked once per
/// record — does this record satisfy the statement's `WHERE` — and each is
/// meaningless to the walk without the other two: the condition to evaluate, the
/// analyzers its searched fields are read with, and where a comparison across
/// two kinds is recorded so the answer can say it happened.
pub(crate) struct Testing<'a> {
    /// The statement's whole condition.
    pub(crate) condition: &'a Expr,
    /// The analyzers the condition's searched fields were resolved with.
    pub(crate) searched: &'a Searched,
    /// Where the evaluator records a comparison across two kinds.
    pub(crate) noticed: &'a Noticed,
}

pub(crate) struct Reporting<'a> {
    /// Notes the source raised.
    pub(crate) collected: &'a mut Vec<Note>,
    /// Where the evaluator records a comparison across two kinds.
    pub(crate) noticed: &'a Noticed,
}

/// What the evaluator can see besides the expression itself.
///
/// The record a condition is being tested against, and what its searched fields
/// need. Both are absent in a value position, where there is no record and
/// nothing to search.
#[derive(Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    /// The record being tested, when there is one.
    pub(crate) record: Option<&'a Value>,
    /// Which record that is, when it is a stored one.
    ///
    /// A record's *value* answers `MATCHES`, because holding a word is a property
    /// of the text alone. A **score** additionally needs what the index knows
    /// about this record — how often it holds each asked term, and how long it is
    /// — and an index is addressed by record id. So the id travels beside the
    /// value rather than being recovered from it.
    ///
    /// Absent where there is no stored record to name: a joined row, a fold's
    /// result, an expression in a value position. Such a row is in no index, and
    /// a score against it is refused for the same reason a score without an index
    /// is.
    pub(crate) id: Option<&'a RecordId>,
    /// The analyzers and collection statistics the searched paths need.
    pub(crate) searched: Option<&'a Searched>,
    /// Where a comparison across two kinds is recorded, when this evaluation is
    /// part of a read that reports notes.
    ///
    /// Borrowed, so it cannot outlive the read — which is the whole reason it
    /// hangs here rather than on the session.
    pub(crate) noticed: Option<&'a Noticed>,
    /// Where this record came in each branch of the fused order that answered
    /// it, when it is being projected by a fused read — what `search::ranks()`
    /// answers, and the reason it answers nowhere else.
    pub(crate) ranks: Option<&'a [Option<u64>]>,
    /// What a `FROM SEARCH` knows about the record it ranked (ADR-0105).
    pub(crate) hit: Option<&'a crate::engine::Hit<'a>>,
}

impl<'a> Scope<'a> {
    /// No record at all.
    ///
    /// For an expression that has none to read: a fold's value substituted into
    /// its projection is arithmetic over a literal, and a path standing beside
    /// one would be a value per record where a value per group belongs — which
    /// the grouping rule refuses before anything runs.
    pub(crate) const fn none() -> Self {
        Self {
            record: None,
            id: None,
            searched: None,
            noticed: None,
            ranks: None,
            hit: None,
        }
    }

    /// A record, with nothing searched.
    pub(crate) const fn of(record: &'a Value) -> Self {
        Self {
            record: Some(record),
            id: None,
            searched: None,
            noticed: None,
            ranks: None,
            hit: None,
        }
    }

    /// A record, and what its searched fields need.
    pub(crate) const fn searching(record: &'a Value, searched: &'a Searched) -> Self {
        Self {
            record: Some(record),
            id: None,
            searched: Some(searched),
            noticed: None,
            ranks: None,
            hit: None,
        }
    }

    /// The same scope, over a record the store can name.
    ///
    /// Left off where the value in scope is not a stored record, which is what
    /// makes the absence meaningful rather than an omission somebody forgot.
    pub(crate) const fn identified(self, id: &'a RecordId) -> Self {
        Self {
            id: Some(id),
            ..self
        }
    }

    /// The same scope, reporting what it compares to this read's notes.
    ///
    /// Added by the read path and left off everywhere else, so an evaluation in
    /// a value position — which has no answer to hang a note on — costs nothing
    /// and says nothing.
    pub(crate) const fn noticing(self, noticed: &'a Noticed) -> Self {
        Self {
            noticed: Some(noticed),
            ..self
        }
    }

    /// The same environment, over this record.
    ///
    /// A `Scope` with no record is what an evaluation needs *besides* the record
    /// — the analyzers, and where to note a crossing — so a walk that evaluates
    /// per record is handed one of those and attaches each record in turn. It is
    /// one parameter where `searched` and `noticed` were two, and it stops the
    /// pair drifting apart at the call sites.
    /// It takes the id as well as the value, so that a scope carrying the
    /// identity of the *previous* record is not a thing this type can hold.
    pub(crate) const fn with(self, id: &'a RecordId, record: &'a Value) -> Self {
        Self {
            record: Some(record),
            id: Some(id),
            ..self
        }
    }

    /// The environment alone: what evaluation needs besides a record.
    pub(crate) const fn over(searched: &'a Searched, noticed: &'a Noticed) -> Self {
        Self {
            record: None,
            id: None,
            searched: Some(searched),
            noticed: Some(noticed),
            ranks: None,
            hit: None,
        }
    }

    /// The same scope, projecting a record of a fused read with its ranks.
    pub(crate) const fn with_ranks(self, ranks: &'a [Option<u64>]) -> Self {
        Self {
            ranks: Some(ranks),
            ..self
        }
    }

    pub(crate) const fn with_hit(self, hit: &'a crate::engine::Hit<'a>) -> Self {
        Self {
            hit: Some(hit),
            ..self
        }
    }

    /// Record a comparison, when this scope is reporting them.
    pub(crate) fn compared(self, left: &Value, right: &Value) {
        if let Some(noticed) = self.noticed {
            noticed.compared(left, right);
        }
    }

    /// The analyzer this path's field declares, if it declares one.
    pub(crate) fn analyzer(self, path: &Path) -> Option<&'a Analyzer> {
        self.searched.and_then(|held| held.analyzer(path))
    }

    /// What this path was ranked against, if it was ranked at all.
    pub(crate) fn ranked(self, path: &Path) -> Option<&'a Ranked> {
        self.searched.and_then(|held| held.ranked(path))
    }

    /// What this read asked of this path, as the rewrite recorded it.
    pub(crate) fn wanted(self, path: &Path) -> &'a [(BinaryOp, String)] {
        self.searched.map_or(&[], |held| held.wanted(path))
    }

    /// The index keeping byte offsets for this path, if a highlight may use it.
    pub(crate) fn offsets(self, path: &Path) -> Option<&'a tessari_storage::IndexDefinition> {
        self.searched.and_then(|held| held.offsets(path))
    }
}
