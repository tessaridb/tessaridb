//! What the model's world holds, and the questions asked of it.

use super::{Decision, Entry, Range, Writer};

/// How far T1's coordinator has got. Its volatile state — the replies it has
/// collected — dies with it; the record it wrote does not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Coordinator {
    /// Nothing written yet.
    Idle,
    /// `B`'s prepare sent; `A`'s staging record and its own prepare not yet
    /// written — one commit in `A`'s range (D13a), racing `B`'s delivery.
    Starting,
    /// `STAGING` written, prepares sent, collecting replies (one per range).
    Waiting { replies: [Option<bool>; 2] },
    /// Every prepare held and the caller told committed; the explicit
    /// decision and `A`'s resolution are still to be written (D14).
    Concluding,
    /// It recorded (or tried to record) a decision and reported it.
    Done,
    /// It died; only the log remembers it.
    Crashed,
}

/// T2, the ordinary single-range commit of key `b`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Second {
    /// Not started.
    Idle,
    /// Read key `b` at the version this writer wrote.
    Read { stamp: Writer },
    /// Committed or refused, having read `stamp`.
    Done { stamp: Writer },
}

/// The reading transaction on the lagging node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub(super) struct Reader {
    /// The version each key was read at, once read.
    pub(super) seen: [Option<Writer>; 2],
    /// Its one decision about T1, once taken (D6).
    pub(super) decided: Option<bool>,
    /// Whether it began after the caller was told T1 committed (D13).
    pub(super) began_after_answer: Option<bool>,
    /// Whether each key was read at its leader — the whole log.
    pub(super) at_leader: [bool; 2],
}

/// The whole world, one point in the exploration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct State {
    /// Each range's log as a majority holds it, starting at the initial version.
    pub(super) logs: [Vec<Entry>; 2],
    pub(super) coordinator: Coordinator,
    /// Prepare messages in flight to each range.
    pub(super) prepares: [u8; 2],
    /// Deliveries spent per range, bounding duplication.
    pub(super) deliveries: [u8; 2],
    /// A participant's answer in flight back to the coordinator.
    pub(super) replies: [Option<bool>; 2],
    pub(super) second: Second,
    pub(super) reader: Reader,
    /// A resolution `B`'s leader has applied and no majority holds yet — its
    /// outcome — lost if that leader dies before it replicates.
    pub(super) tail: Option<bool>,
    /// What the caller was told: `Some(true)` committed, `Some(false)` not.
    pub(super) told: Option<bool>,
    /// Whether T1 was ever committed implicitly: its record `STAGING` while
    /// every participant's prepare stood (D14). Bookkeeping of the model.
    pub(super) implicit: bool,
    /// `B`'s leader pruned its log past the bar, which went with the record
    /// that wrote it (ADR-0119).
    pub(super) pruned: bool,
}

impl State {
    pub(super) fn initial() -> Self {
        Self {
            logs: [
                vec![Entry::Version(Writer::Initial)],
                vec![Entry::Version(Writer::Initial)],
            ],
            coordinator: Coordinator::Idle,
            prepares: [0, 0],
            deliveries: [0, 0],
            replies: [None, None],
            second: Second::Idle,
            reader: Reader::default(),
            tail: None,
            told: None,
            implicit: false,
            pruned: false,
        }
    }

    pub(super) fn log(&self, range: Range) -> &[Entry] {
        &self.logs[range.slot()]
    }

    /// The record's current state at the leader of `A`, if written.
    pub(super) fn record(&self) -> Option<Decision> {
        record_in(self.log(Range::A))
    }

    /// Whether the record took both outcomes at some point in its life.
    pub(super) fn two_outcomes(&self) -> bool {
        let log = self.log(Range::A);
        log.contains(&Entry::Record(Decision::Committed))
            && log.contains(&Entry::Record(Decision::Aborted))
    }

    /// The outcome the record took, whether or not it was forgotten since.
    pub(super) fn outcome(&self) -> Option<Decision> {
        self.log(Range::A)
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Entry::Record(decision @ (Decision::Committed | Decision::Aborted)) => {
                    Some(*decision)
                }
                _ => None,
            })
    }

    /// Whether the record has been forgotten.
    pub(super) fn forgotten(&self) -> bool {
        self.log(Range::A).contains(&Entry::Forget)
    }

    /// Whether T1 is committed implicitly right now: its record `STAGING` and
    /// an intent standing in every range (D14).
    pub(super) fn committed_implicitly(&self) -> bool {
        self.record() == Some(Decision::Staging)
            && Range::BOTH
                .iter()
                .all(|range| intent_stands(self.log(*range)))
    }

    /// Whether an intent stands as `range`'s leader sees it: its log, and for
    /// `B` the resolution only the leader holds.
    pub(super) fn intent_stands_at_leader(&self, range: Range) -> bool {
        intent_stands(self.log(range)) && (range == Range::A || self.tail.is_none())
    }
}

/// The record's state in a prefix of `A`'s log.
pub(super) fn record_in(log: &[Entry]) -> Option<Decision> {
    log.iter().rev().find_map(|entry| match entry {
        Entry::Record(decision) => Some(Some(*decision)),
        Entry::Forget => Some(None),
        _ => None,
    })?
}

/// One version of a key as a prefix of its log shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shown {
    /// An ordinary version.
    Plain(Writer),
    /// T1's intent, not yet resolved.
    Intent,
    /// T1's intent, resolved as committed.
    Resolved,
}

/// The versions a prefix of one range's log shows, oldest first. A dropped
/// intent shows nothing; a resolved one sits where its intent was written.
pub(super) fn versions(log: &[Entry]) -> Vec<Shown> {
    let mut shown: Vec<Shown> = Vec::with_capacity(log.len());
    for entry in log {
        match entry {
            Entry::Version(writer) => shown.push(Shown::Plain(*writer)),
            Entry::Intent { .. } => shown.push(Shown::Intent),
            Entry::Resolved { committed } => {
                if let Some(at) = shown.iter().rposition(|version| *version == Shown::Intent) {
                    if *committed {
                        shown[at] = Shown::Resolved;
                    } else {
                        shown.remove(at);
                    }
                }
            }
            Entry::Record(_) | Entry::Forget | Entry::Prevent => {}
        }
    }
    shown
}

/// Whether a prefix of a log holds T1's intent (resolved or not).
pub(super) fn holds_intent(log: &[Entry]) -> bool {
    log.iter()
        .any(|entry| matches!(entry, Entry::Intent { .. }))
}

/// Whether a log holds an intent that no resolution has followed yet.
pub(super) fn intent_stands(log: &[Entry]) -> bool {
    versions(log).contains(&Shown::Intent)
}

/// The writer of the latest version a participant can vouch for on its own:
/// ordinary versions and resolved intents. A standing intent is not a version.
pub(super) fn latest_committed(log: &[Entry]) -> Writer {
    versions(log)
        .iter()
        .rev()
        .find_map(|version| match version {
            Shown::Plain(writer) => Some(*writer),
            Shown::Resolved => Some(Writer::T1),
            Shown::Intent => None,
        })
        .unwrap_or(Writer::Initial)
}

/// The key's committed history, oldest first, given T1's final decision.
pub(super) fn history(log: &[Entry], t1_committed: bool) -> Vec<Writer> {
    versions(log)
        .iter()
        .filter_map(|version| match version {
            Shown::Plain(writer) => Some(*writer),
            Shown::Resolved => Some(Writer::T1),
            Shown::Intent => t1_committed.then_some(Writer::T1),
        })
        .collect()
}
