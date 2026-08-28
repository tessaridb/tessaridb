//! Failures raised while reading a script.
//!
//! Every variant carries a [`Span`], because a message that cannot point at the
//! offending characters leaves the author of a hand-written query re-reading it
//! and guessing.

use crate::function::Function;
use crate::token::Span;

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure reading a script.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A character that starts nothing the language knows.
    #[error("unexpected character {found:?} at {span}")]
    UnexpectedCharacter {
        /// The character found.
        found: char,
        /// Where it is.
        span: Span,
    },

    /// A string literal reached the end of the script without closing.
    #[error("unterminated string starting at {span}")]
    UnterminatedString {
        /// Where the string began.
        span: Span,
    },

    /// A backslash escape this language does not define.
    ///
    /// Refused rather than passed through: a script that writes `\d` meaning a
    /// literal backslash and a `d` would otherwise silently lose the backslash,
    /// and the value stored would differ from the value written.
    #[error("invalid escape \\{found} at {span}")]
    InvalidEscape {
        /// The character after the backslash.
        found: char,
        /// Where the escape is.
        span: Span,
    },

    /// A shape literal whose object is not a shape.
    ///
    /// The reason comes from the geometry reader rather than being reworded
    /// here, so a caller reading "a position is two numbers, longitude first"
    /// reads the same sentence whichever surface refused their shape.
    #[error("{reason}, in the shape at {span}")]
    MalformedGeometry {
        /// What the reader said.
        reason: String,
        /// Where the literal is.
        span: Span,
    },

    /// A shape literal holding something that is not a written-out value.
    ///
    /// A literal is read at parse time, so every part of it has to be known
    /// then. A field, a parameter or a call inside one would have to be
    /// evaluated, and a shape that could change per record is not a literal.
    #[error("a shape literal is written out in full, and {found} at {span} is not")]
    ComputedGeometry {
        /// What stood where a written-out value belonged.
        found: &'static str,
        /// Where it is.
        span: Span,
    },

    /// A number the language cannot represent.
    #[error("{text:?} at {span} is not a number this store can hold")]
    InvalidNumber {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A byte literal that is not whole bytes of hexadecimal.
    #[error("{reason} in byte literal at {span}")]
    InvalidBytes {
        /// What was wrong: an odd digit count, or a non-hexadecimal digit.
        reason: &'static str,
        /// Where it is.
        span: Span,
    },

    /// A duration with an unknown unit, or one too large to represent.
    #[error("{text:?} at {span} is not a duration this store can hold")]
    InvalidDuration {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A token the grammar does not allow where it stands.
    #[error("expected {expected} at {span}, found {found}")]
    UnexpectedToken {
        /// What was there, as the language spells it.
        found: String,
        /// What would have been accepted.
        expected: &'static str,
        /// Where it is.
        span: Span,
    },

    /// The script ended in the middle of a statement.
    #[error("expected {expected}, but the script ended at {span}")]
    UnexpectedEnd {
        /// What would have been accepted.
        expected: &'static str,
        /// The end of the source.
        span: Span,
    },

    /// Something the language will have and this milestone does not.
    ///
    /// Separate from [`Error::UnexpectedToken`] because the two say different
    /// things to whoever reads them: one is a typo, the other is a feature that
    /// is absent on purpose, and telling an author to check their spelling when
    /// the answer is "not yet" wastes their afternoon.
    #[error("{feature} is not in this milestone (at {span})")]
    Unsupported {
        /// The absent feature, named as `docs/tessariql.md` §8 names it.
        feature: &'static str,
        /// Where it was asked for.
        span: Span,
    },

    /// Text after `datetime` that is not an instant.
    #[error("{text:?} at {span} is not an instant: expected RFC 3339, as in 1970-01-01T00:00:00Z")]
    InvalidDatetime {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// Text after `uuid` that is not sixteen bytes.
    #[error("{text:?} at {span} is not a uuid")]
    InvalidUuid {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A number after `dec` that no exact decimal can hold.
    #[error("{text:?} at {span} is not a decimal this store can hold exactly")]
    InvalidDecimal {
        /// The text as written.
        text: String,
        /// Where it is.
        span: Span,
    },

    /// A record identity that is not one of the four kinds a record id has.
    #[error("a record id is an integer, text, a uuid or bytes (at {span})")]
    InvalidRecordId {
        /// Where the identity was written.
        span: Span,
    },

    /// `RANGE` followed by something that is not a range.
    #[error("expected a range such as 'a'..'m' at {span}")]
    NotARange {
        /// Where the expression was written.
        span: Span,
    },

    /// An object literal that names one field twice.
    ///
    /// Refused rather than resolved: keeping either occurrence stores a value
    /// the author did not write, and nothing downstream can tell which one was
    /// meant.
    #[error("field {name:?} is written twice in one object (at {span})")]
    DuplicateField {
        /// The field's name.
        name: String,
        /// Where the second occurrence is.
        span: Span,
    },

    /// A `TIMEOUT` names a ceiling no statement could satisfy.
    ///
    /// `TIMEOUT 0s` and `TIMEOUT -5s` can only refuse, whatever the read does and
    /// however fast the store is. A clause that can only refuse is a mistake in
    /// the statement rather than a budget, so it is caught where the statement is
    /// read — before a scan runs to be refused by it.
    #[error("a timeout of {written} (at {span}) can only refuse; a ceiling is a positive duration")]
    EmptyTimeout {
        /// The ceiling as it was written.
        written: String,
        /// Where the clause is.
        span: Span,
    },

    /// Two projections in one read answer under the same name.
    ///
    /// `SELECT address.city, work.city` would write one field twice into a
    /// name-ordered object and keep whichever came last, so the read would
    /// quietly return half of what it asked for. Refused here rather than at
    /// execution because it is a property of the statement.
    #[error("two projections answer under the name {name:?} (at {span}); one of them needs `AS`")]
    DuplicateProjection {
        /// The name they share.
        name: String,
        /// Where the second one is.
        span: Span,
    },

    /// A `group::name(…)` naming no function this language has.
    #[error("there is no function called {name} (at {span})")]
    NoSuchFunction {
        /// The name as written.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// A call with the wrong number of arguments.
    ///
    /// Refused when the statement is read rather than when it runs: the set of
    /// functions is known then, so this is a mistake that never needs a record
    /// to see.
    #[error("{function} takes {expected} argument(s), not {found} (at {span})")]
    WrongArity {
        /// The function called.
        function: Function,
        /// How many it takes.
        expected: usize,
        /// How many were written.
        found: usize,
        /// Where the call is.
        span: Span,
    },

    /// A grouped read projecting something that is neither a key nor a fold.
    ///
    /// The value has as many answers as the group has records, and picking one
    /// silently is how a wrong number reaches a report.
    #[error(
        "{name:?} is neither a group key nor a fold, so a grouped read cannot answer with it (at {span})"
    )]
    UngroupedProjection {
        /// The projection's name.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// `*` used where a fold needs a value.
    ///
    /// `count(*)` counts records; `sum(*)` would have to invent what it is
    /// summing.
    #[error(
        "`*` means the records themselves, which only `count` can fold — not `{fold}` (at {span})"
    )]
    StarIsOnlyForCount {
        /// The fold as written.
        fold: &'static str,
        /// Where the call is.
        span: Span,
    },

    /// `[*]` written where several values have no rule.
    ///
    /// A route holding `[*]` denotes the values it reaches rather than a value,
    /// and each context needs its own rule for what to do with several: a
    /// comparison holds when any of them satisfies it, a projection answers with
    /// all of them, an index keeps one entry per element. Everywhere else — a
    /// group key, an ordering, a function's argument, a `FETCH` route, either
    /// side of a comparison but the left — is refused **by name**, because
    /// half-building one would answer a question about elements with an answer
    /// about arrays.
    #[error("`[*]` reaches several values, and nothing here has a rule for several (at {span})")]
    SeveralOutsideAComparison {
        /// Where the route was written.
        span: Span,
    },

    /// An `ASSERT` that is not a constraint the store can check.
    ///
    /// An assertion is checked on the store's apply path, where the verdict must
    /// be a pure function of the record so that a replica reaches the same one.
    /// So it is a **closed vocabulary** — `$value` compared against a literal,
    /// combined with `AND`, `OR` and `NOT` — and everything else is refused
    /// where it is written rather than where it would run.
    #[error(
        "an ASSERT compares `$value` against a written value, combined with AND, OR and NOT; \
         a call, an arithmetic expression, a field or another parameter is not checkable \
         where validation lives (at {span})"
    )]
    AssertionNotAConstraint {
        /// Where the offending part was written.
        span: Span,
    },

    /// A `UNIQUE` index over a multi-valued route.
    ///
    /// It has two readings and the store must not pick one silently: *no two
    /// records share an element*, or *a record's own elements are distinct*.
    /// They refuse different writes, so a caller who meant one and got the other
    /// finds out from a rejected write months later.
    #[error(
        "a UNIQUE index over `[*]` could mean no two records share an element or that \
         a record's own elements are distinct, and this store will not pick one (at {span})"
    )]
    SeveralInAUniqueIndex {
        /// Where the route was written.
        span: Span,
    },

    /// A `SEARCH` or vector index over a multi-valued route.
    ///
    /// Both already decide their own multiplicity — a search index turns text
    /// into as many postings as it has terms, and a vector index needs exactly
    /// one vector per record to place a node in its graph. `[*]` on top of that
    /// is a second multiplicity with no stated rule.
    #[error(
        "a SEARCH or VECTOR index decides its own multiplicity, so `[*]` has no meaning \
         in one (at {span})"
    )]
    SeveralInAnAnalysedIndex {
        /// Where the route was written.
        span: Span,
    },

    /// More than one multi-valued route in one index.
    ///
    /// The entries would be the product of the two reaches — one per pair — paid
    /// on every write to the record, and "one entry per element" stops having one
    /// meaning when there are two sets of elements to be per.
    #[error(
        "an index may hold one `[*]` route; two would keep an entry per pair of elements \
         (at {span})"
    )]
    SeveralRoutesInOneIndex {
        /// Where the second route was written.
        span: Span,
    },

    /// A fold folding over another fold.
    ///
    /// `mean(sum(price))` has no meaning at one grouping level: the inner fold
    /// has already collapsed the records the outer one would fold over, so what
    /// is left to average is one number. Refused where the statement is read,
    /// because nothing has to run for it to be wrong.
    #[error("a fold cannot fold over another fold (at {span})")]
    FoldInsideAFold {
        /// Where the outer fold is.
        span: Span,
    },

    /// A fold standing in a filter rather than a projection.
    ///
    /// A filter over *groups* is a second filter position with its own scoping
    /// rule — it sees folds where `WHERE` does not — and this language does not
    /// have one yet.
    #[error(
        "a fold filters groups rather than records; `WHERE` sees one record at a time (at {span})"
    )]
    FoldInAFilter {
        /// Where the fold is.
        span: Span,
    },

    /// A projected path ends in a position, so it has no name of its own.
    ///
    /// A projection is named by the last step of its path, and `[0]` is not a
    /// name. Every invented spelling — `tags_0`, `tags`, `_0` — is a convention
    /// the author would have to learn from a surprise.
    #[error("the projection at {span} ends in a position and has no name; add `AS <name>`")]
    UnnamedProjection {
        /// Where the projection is.
        span: Span,
    },

    /// A join key that does not name one of the two tables being joined.
    ///
    /// The two sides of `ON` are routes into the joined row, and the joined row
    /// has exactly two names in it. A root that is neither is either a typo or a
    /// third table nobody asked for.
    #[error(
        "`{root}` is not `{left}` or `{right}`, which are the two sides of this join (at {span})"
    )]
    NotASideOfTheJoin {
        /// The root as written.
        root: String,
        /// The left table's name.
        left: String,
        /// The right table's name.
        right: String,
        /// Where it was written.
        span: Span,
    },

    /// Both sides of `ON` named the same table.
    ///
    /// A join needs two sides to tell apart, and two records under one name is
    /// not a row anybody can read. Joining a table to itself needs aliases,
    /// which is a language surface rather than a clause.
    #[error(
        "both sides of this `ON` name `{name}`; a join needs one route into each side (at {span})"
    )]
    OneSidedJoin {
        /// The table both sides named.
        name: String,
        /// Where the `ON` is.
        span: Span,
    },

    /// A join key whose first step is a position rather than a field.
    ///
    /// `ON users[0] = …` names the table and then indexes it, and a table is not
    /// an array. A join key is a route into one record.
    #[error(
        "a join key names a field of the record, and `{root}` is followed by a position (at {span})"
    )]
    JoinKeyIsNotAField {
        /// The root as written.
        root: String,
        /// Where it was written.
        span: Span,
    },

    /// A parameter supplying something a record cannot be identified by.
    ///
    /// A record's identity is an integer, text, a uuid or bytes. A float or an
    /// object is refused where it is supplied rather than converted into text,
    /// which would quietly make `1.0` and `'1.0'` the same record.
    #[error("`${name}` at {span} holds {found}, which a record cannot be identified by")]
    NotARecordIdentity {
        /// The parameter's name, without its marker.
        name: String,
        /// What it held.
        found: &'static str,
        /// Where the identity was written.
        span: Span,
    },

    /// A statement [`crate::render`] does not write back out as text.
    ///
    /// The renderer covers `SELECT` at this milestone. Every other form names
    /// itself here rather than falling into a catch-all, so the boundary is
    /// something a caller reads in a message instead of discovering as silence.
    #[error("{statement} is not written back out at this milestone (at {span})")]
    Unrenderable {
        /// The statement form, as it is spelled.
        statement: &'static str,
        /// Where it sits in the source.
        span: Span,
    },

    /// A parameter the caller did not supply a value for.
    ///
    /// Refused while binding, which is before the first statement runs — so a
    /// script whose last statement names an unbound parameter writes nothing at
    /// all. Failing where the value is reached instead would leave a
    /// half-applied script behind, which is the state the store's own
    /// transaction rules exist to prevent.
    #[error("no value was supplied for the parameter `${name}` at {span}")]
    UnboundParameter {
        /// The parameter's name, without its marker.
        name: String,
        /// Where it was written.
        span: Span,
    },

    /// Two `LET`s in one script bind the same name.
    ///
    /// Refused rather than shadowed. A binding is substituted into the
    /// statements below it, so two of them would make `$x` mean one value in
    /// part of the script and another value further down — and a reader would
    /// have to count statements to know which. One name, one value, everywhere
    /// it is written.
    #[error("`${name}` is bound twice in one script (at {span})")]
    BoundTwice {
        /// The name they share, without its marker.
        name: String,
        /// Where the second `LET` is.
        span: Span,
    },

    /// A `LET` binds a name the caller also supplied a value for.
    ///
    /// Refused because there is no reading of it that is not surprising: either
    /// the caller's value is silently discarded, or the script's own binding is.
    /// The caller drops the entry or the script picks another name.
    #[error("`${name}` was supplied by the caller and is also bound by the script (at {span})")]
    BindingCollidesWithParameter {
        /// The name they share, without its marker.
        name: String,
        /// Where the `LET` is.
        span: Span,
    },

    /// A script names its answer twice.
    ///
    /// `RETURN` says which value the script answers with, so two of them make
    /// "the answer" depend on which one ran. A property of the statement text,
    /// so it is refused where it is written.
    #[error("a script answers with one value, and this one has two `RETURN`s (at {span})")]
    ReturnedTwice {
        /// Where the second `RETURN` is.
        span: Span,
    },
}

impl Error {
    /// Where the failure is, for a caller rendering a caret.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::UnexpectedCharacter { span, .. }
            | Self::UnterminatedString { span }
            | Self::InvalidEscape { span, .. }
            | Self::InvalidNumber { span, .. }
            | Self::InvalidBytes { span, .. }
            | Self::InvalidDuration { span, .. }
            | Self::UnexpectedToken { span, .. }
            | Self::UnexpectedEnd { span, .. }
            | Self::Unsupported { span, .. }
            | Self::InvalidDatetime { span, .. }
            | Self::InvalidUuid { span, .. }
            | Self::InvalidDecimal { span, .. }
            | Self::InvalidRecordId { span }
            | Self::NotARange { span }
            | Self::DuplicateField { span, .. }
            | Self::UngroupedProjection { span, .. }
            | Self::StarIsOnlyForCount { span, .. }
            | Self::SeveralOutsideAComparison { span }
            | Self::AssertionNotAConstraint { span }
            | Self::SeveralInAUniqueIndex { span }
            | Self::SeveralInAnAnalysedIndex { span }
            | Self::SeveralRoutesInOneIndex { span }
            | Self::FoldInsideAFold { span }
            | Self::FoldInAFilter { span }
            | Self::NoSuchFunction { span, .. }
            | Self::WrongArity { span, .. }
            | Self::DuplicateProjection { span, .. }
            | Self::UnnamedProjection { span }
            | Self::NotASideOfTheJoin { span, .. }
            | Self::OneSidedJoin { span, .. }
            | Self::JoinKeyIsNotAField { span, .. }
            | Self::UnboundParameter { span, .. }
            | Self::BoundTwice { span, .. }
            | Self::BindingCollidesWithParameter { span, .. }
            | Self::ReturnedTwice { span }
            | Self::NotARecordIdentity { span, .. }
            | Self::Unrenderable { span, .. }
            | Self::MalformedGeometry { span, .. }
            | Self::ComputedGeometry { span, .. }
            | Self::EmptyTimeout { span, .. } => *span,
        }
    }
}
