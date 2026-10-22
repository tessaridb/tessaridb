//! The writes that change a container held under one space key (G079 C6): a
//! list, a hash, a set of members, and members ranked by score.
//!
//! Each is ONE statement that reads the key and writes it, so it is atomic the
//! way `INCR` is — two writers of one version are a conflict the first
//! committer wins, and a lone statement is run again rather than lost.

use super::{Expr, RecordTarget};

/// Which end of a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListEnd {
    /// The first element.
    Front,
    /// The last element.
    Back,
}

/// A condition on whether a member is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Only members not yet there.
    Absent,
    /// Only members already there.
    Present,
}

/// A condition on how a new score compares with the one held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreCompare {
    /// Only when the new score is greater.
    Greater,
    /// Only when the new score is less.
    Less,
}

/// When a `HASH EXPIRE` gives a field its instant (ADR-0132).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldExpiryCondition {
    /// `IF NONE` — only a field with no instant.
    None,
    /// `IF SOME` — only a field that has one.
    Some,
    /// `IF LATER` — only an instant later than the field's; a field with none
    /// is later than any.
    Later,
    /// `IF SOONER` — only an instant sooner than the field's.
    Sooner,
}

/// One end of a score or member range: a value, or `NONE` for no end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeEnd {
    /// Where the range stops; `NONE` leaves this side open.
    pub value: Expr,
    /// The end itself is not in the range.
    pub exclusive: bool,
}

/// Which members of a ranking a range names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreRange {
    /// `RANK FROM a TO b` — positions from the lowest, both kept, either
    /// counted from the highest when negative.
    Rank {
        /// The first position.
        start: Expr,
        /// The last position.
        stop: Expr,
    },
    /// `SCORE FROM lo TO hi` — the members whose score is between.
    Score {
        /// The low end.
        low: RangeEnd,
        /// The high end.
        high: RangeEnd,
    },
    /// `MEMBER FROM lo TO hi` — the members whose text is between, which
    /// orders them as a ranking of equal scores does.
    Member {
        /// The low end.
        low: RangeEnd,
        /// The high end.
        high: RangeEnd,
    },
}

impl ScoreRange {
    fn expressions(&self) -> Vec<&Expr> {
        match self {
            Self::Rank { start, stop } => vec![start, stop],
            Self::Score { low, high } | Self::Member { low, high } => vec![&low.value, &high.value],
        }
    }

    fn expressions_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Self::Rank { start, stop } => vec![start, stop],
            Self::Score { low, high } | Self::Member { low, high } => {
                vec![&mut low.value, &mut high.value]
            }
        }
    }
}

/// The four kinds of container a key may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// An array.
    List,
    /// An object of fields.
    Hash,
    /// A set of members.
    Members,
    /// A set of `[score, member]` pairs, ordered by score.
    Scores,
}

impl Container {
    /// The kind as a refusal names it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Hash => "hash",
            Self::Members => "set",
            Self::Scores => "sorted set",
        }
    }
}

/// What a structure statement does to the container under its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructureOp {
    /// `LIST PUSH k FRONT|BACK v, … [IF PRESENT]` — answers the new length.
    ListPush {
        /// Where the values go; at the front each goes first in turn.
        end: ListEnd,
        /// The values, in the order written.
        values: Vec<Expr>,
        /// Only when the list is already there.
        if_present: bool,
    },
    /// `LIST POP k FRONT|BACK [LIMIT n]` — the value, or an array with a limit.
    ListPop {
        /// The end taken from.
        end: ListEnd,
        /// How many at most; absent takes one and answers it bare.
        limit: Option<Expr>,
    },
    /// `LIST SET k AT i = v` — replace one element; a negative index counts
    /// from the back.
    ListSet {
        /// The position.
        index: Expr,
        /// The new element.
        value: Expr,
    },
    /// `LIST TRIM k FROM a TO b` — keep the elements from `a` to `b`, both
    /// included, either counted from the back when negative.
    ListTrim {
        /// The first kept.
        start: Expr,
        /// The last kept.
        stop: Expr,
    },
    /// `LIST REMOVE k v [LIMIT n] [FROM BACK]` — answers how many went.
    ListRemove {
        /// The element removed.
        value: Expr,
        /// How many at most; absent removes every one.
        limit: Option<Expr>,
        /// Counted from the back rather than the front.
        from_back: bool,
    },
    /// `LIST INSERT k v BEFORE|AFTER pivot` — answers the new length, `-1`
    /// when the pivot is not there and `0` when the list is not.
    ListInsert {
        /// The element inserted.
        value: Expr,
        /// After the pivot rather than before it.
        after: bool,
        /// The element it goes beside, the first such.
        pivot: Expr,
    },
    /// `LIST MOVE k FRONT|BACK TO k2 FRONT|BACK` — answers the element moved.
    ListMove {
        /// The end of this list taken from.
        from: ListEnd,
        /// The list it goes to.
        to: RecordTarget,
        /// The end it goes to.
        to_end: ListEnd,
    },
    /// `HASH SET k f = v, … [IF ABSENT]` — answers how many fields are new.
    HashSet {
        /// Each field and its value.
        fields: Vec<(Expr, Expr)>,
        /// Only fields not yet there.
        if_absent: bool,
    },
    /// `HASH REMOVE k f, …` — answers how many went.
    HashRemove {
        /// The fields.
        fields: Vec<Expr>,
    },
    /// `HASH INCR k f BY n` — answers the field's new number.
    HashIncr {
        /// The field.
        field: Expr,
        /// How much to add.
        by: Expr,
    },
    /// `HASH EXPIRE k f, … AT when [IF NONE|SOME|LATER|SOONER]` — answers per
    /// field `-2` not there, `0` its condition unmet, `1` given the instant,
    /// `2` removed because the instant is not in the future (ADR-0132).
    HashExpire {
        /// The fields.
        fields: Vec<Expr>,
        /// A datetime, or a duration from now.
        at: Expr,
        /// When to give it.
        condition: Option<FieldExpiryCondition>,
    },
    /// `HASH TTL k f, …` — answers per field the time left, `NULL` for a field
    /// with no instant and `NONE` for no field. A read.
    HashTtl {
        /// The fields.
        fields: Vec<Expr>,
    },
    /// `HASH PERSIST k f, …` — answers per field `-2` not there, `-1` no
    /// instant, `1` its instant cleared.
    HashPersist {
        /// The fields.
        fields: Vec<Expr>,
    },
    /// `MEMBERS ADD k v, …` — answers how many are new.
    MembersAdd {
        /// The members.
        values: Vec<Expr>,
    },
    /// `MEMBERS REMOVE k v, …` — answers how many went.
    MembersRemove {
        /// The members.
        values: Vec<Expr>,
    },
    /// `MEMBERS POP k [LIMIT n]` — members chosen at random, taken.
    MembersPop {
        /// How many at most; absent takes one and answers it bare.
        limit: Option<Expr>,
    },
    /// `MEMBERS MOVE k v TO k2` — answers whether it moved.
    MembersMove {
        /// The member.
        value: Expr,
        /// The set it goes to.
        to: RecordTarget,
    },
    /// `SCORES ADD k m = s, … [IF ABSENT|IF PRESENT] [IF GREATER|IF LESS]
    /// [CHANGED]` — answers how many are new, or with `CHANGED` how many were
    /// added or given another score.
    ScoresAdd {
        /// Each member and its score.
        scores: Vec<(Expr, Expr)>,
        /// Only members not yet there, or only those already there.
        presence: Option<Presence>,
        /// Only scores that move this way.
        compare: Option<ScoreCompare>,
        /// Count changed scores too.
        changed: bool,
    },
    /// `SCORES INCR k m BY n` — answers the member's new score.
    ScoresIncr {
        /// The member.
        member: Expr,
        /// How much to add.
        by: Expr,
    },
    /// `SCORES REMOVE k m, …` — answers how many went.
    ScoresRemove {
        /// The members.
        members: Vec<Expr>,
    },
    /// `SCORES REMOVE k RANK|SCORE|MEMBER FROM … TO …` — answers how many went.
    ScoresRemoveRange {
        /// The members removed.
        range: ScoreRange,
    },
    /// `SCORES POP k LOWEST|HIGHEST [LIMIT n]` — answers `[[member, score], …]`.
    ScoresPop {
        /// From the highest score rather than the lowest.
        highest: bool,
        /// How many at most; absent takes one.
        limit: Option<Expr>,
    },
}

impl StructureOp {
    /// The kind of container this writes.
    pub const fn container(&self) -> Container {
        match self {
            Self::ListPush { .. }
            | Self::ListPop { .. }
            | Self::ListSet { .. }
            | Self::ListTrim { .. }
            | Self::ListRemove { .. }
            | Self::ListInsert { .. }
            | Self::ListMove { .. } => Container::List,
            Self::HashSet { .. }
            | Self::HashRemove { .. }
            | Self::HashIncr { .. }
            | Self::HashExpire { .. }
            | Self::HashTtl { .. }
            | Self::HashPersist { .. } => Container::Hash,
            Self::MembersAdd { .. }
            | Self::MembersRemove { .. }
            | Self::MembersPop { .. }
            | Self::MembersMove { .. } => Container::Members,
            Self::ScoresAdd { .. }
            | Self::ScoresIncr { .. }
            | Self::ScoresRemove { .. }
            | Self::ScoresRemoveRange { .. }
            | Self::ScoresPop { .. } => Container::Scores,
        }
    }

    /// Whether the statement only reads its key.
    pub const fn reads_only(&self) -> bool {
        matches!(self, Self::HashTtl { .. })
    }

    /// The second key a move writes, if this is one.
    pub const fn second_key(&self) -> Option<&RecordTarget> {
        match self {
            Self::ListMove { to, .. } | Self::MembersMove { to, .. } => Some(to),
            _ => None,
        }
    }

    /// The second key a move writes, mutably.
    pub const fn second_key_mut(&mut self) -> Option<&mut RecordTarget> {
        match self {
            Self::ListMove { to, .. } | Self::MembersMove { to, .. } => Some(to),
            _ => None,
        }
    }

    /// Every expression the statement evaluates, in the order written.
    pub fn expressions(&self) -> Vec<&Expr> {
        match self {
            Self::ListPush { values, .. }
            | Self::MembersAdd { values }
            | Self::MembersRemove { values } => values.iter().collect(),
            Self::HashRemove { fields }
            | Self::HashTtl { fields }
            | Self::HashPersist { fields } => fields.iter().collect(),
            Self::HashExpire { fields, at, .. } => {
                fields.iter().chain(std::iter::once(at)).collect()
            }
            Self::ScoresRemove { members } => members.iter().collect(),
            Self::ScoresRemoveRange { range } => range.expressions(),
            Self::ListPop { limit, .. }
            | Self::MembersPop { limit }
            | Self::ScoresPop { limit, .. } => limit.iter().collect(),
            Self::ListSet { index, value } => vec![index, value],
            Self::ListTrim { start, stop } => vec![start, stop],
            Self::ListRemove { value, limit, .. } => {
                std::iter::once(value).chain(limit.iter()).collect()
            }
            Self::ListInsert { value, pivot, .. } => vec![value, pivot],
            Self::ListMove { .. } => Vec::new(),
            Self::HashSet { fields, .. } => pairs(fields),
            Self::ScoresAdd { scores, .. } => pairs(scores),
            Self::HashIncr { field, by } => vec![field, by],
            Self::ScoresIncr { member, by } => vec![member, by],
            Self::MembersMove { value, .. } => vec![value],
        }
    }

    /// Every expression the statement evaluates, mutably.
    pub fn expressions_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Self::ListPush { values, .. }
            | Self::MembersAdd { values }
            | Self::MembersRemove { values } => values.iter_mut().collect(),
            Self::HashRemove { fields }
            | Self::HashTtl { fields }
            | Self::HashPersist { fields } => fields.iter_mut().collect(),
            Self::HashExpire { fields, at, .. } => {
                fields.iter_mut().chain(std::iter::once(at)).collect()
            }
            Self::ScoresRemove { members } => members.iter_mut().collect(),
            Self::ScoresRemoveRange { range } => range.expressions_mut(),
            Self::ListPop { limit, .. }
            | Self::MembersPop { limit }
            | Self::ScoresPop { limit, .. } => limit.iter_mut().collect(),
            Self::ListSet { index, value } => vec![index, value],
            Self::ListTrim { start, stop } => vec![start, stop],
            Self::ListRemove { value, limit, .. } => {
                std::iter::once(value).chain(limit.iter_mut()).collect()
            }
            Self::ListInsert { value, pivot, .. } => vec![value, pivot],
            Self::ListMove { .. } => Vec::new(),
            Self::HashSet { fields, .. } => pairs_mut(fields),
            Self::ScoresAdd { scores, .. } => pairs_mut(scores),
            Self::HashIncr { field, by } => vec![field, by],
            Self::ScoresIncr { member, by } => vec![member, by],
            Self::MembersMove { value, .. } => vec![value],
        }
    }
}

fn pairs(pairs: &[(Expr, Expr)]) -> Vec<&Expr> {
    pairs
        .iter()
        .flat_map(|(name, value)| [name, value])
        .collect()
}

fn pairs_mut(pairs: &mut [(Expr, Expr)]) -> Vec<&mut Expr> {
    pairs
        .iter_mut()
        .flat_map(|(name, value)| [name, value])
        .collect()
}
