//! Running a corpus, and saying precisely what failed.

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::{Expr, ExprKind, StatementKind, parse};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

use crate::case::{Case, Corpus, Expectation};

/// What one case did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    /// The corpus the case came from.
    pub corpus: String,
    /// What the case is called.
    pub name: String,
    /// The line it began on.
    pub line: usize,
    /// Why it failed, or nothing if it passed.
    pub failure: Option<String>,
}

impl CaseResult {
    /// Whether the case did what it said it would.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

/// Run every case of a corpus against one fresh store.
///
/// The store is fresh per corpus and shared **within** it, because a corpus file
/// is a session: a case may read what an earlier case wrote, which is how the
/// language is used and what a suite of isolated one-statement cases never
/// exercises.
#[must_use]
pub fn run(corpus: &Corpus) -> Vec<CaseResult> {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let Ok(store) = Store::open(backend) else {
        return vec![CaseResult {
            corpus: corpus.name.clone(),
            name: "<the store>".to_owned(),
            line: 0,
            failure: Some("the store would not open".to_owned()),
        }];
    };
    let mut session = Session::new(&store);
    corpus
        .cases
        .iter()
        .map(|case| CaseResult {
            corpus: corpus.name.clone(),
            name: case.name.clone(),
            line: case.line,
            failure: check(&mut session, case),
        })
        .collect()
}

/// Run one case and compare what happened to what it expected.
fn check(session: &mut Session<'_>, case: &Case) -> Option<String> {
    let outcome = session.run(&case.script);
    match (&case.expectation, outcome) {
        (Expectation::Ok, Ok(_)) => None,
        (Expectation::Ok, Err(error)) => Some(format!("expected it to run; it failed: {error}")),
        (Expectation::Error(wanted), Err(error)) => {
            let found = kind_name(&error);
            if found == wanted {
                return None;
            }
            Some(format!(
                "expected {wanted}; failed with {found} instead: {error}"
            ))
        }
        (Expectation::Error(wanted), Ok(_)) => {
            Some(format!("expected {wanted}; it ran without failing"))
        }
        (wanted, Ok(outcomes)) => compare(wanted, outcomes.last()),
        (wanted, Err(error)) => Some(format!("expected {wanted:?}; it failed: {error}")),
    }
}

fn compare(wanted: &Expectation, last: Option<&Outcome>) -> Option<String> {
    let Some(outcome) = last else {
        return Some("the script has no statements to answer".to_owned());
    };
    match wanted {
        Expectation::Rows(count) => match outcome.records() {
            Some(records) if records.len() == *count => None,
            Some(records) => Some(format!("expected {count} rows, found {}", records.len())),
            None => Some(format!("expected rows; the answer was {outcome:?}")),
        },
        Expectation::Keys(count) => match outcome.keys() {
            Some(keys) if keys.len() == *count => None,
            Some(keys) => Some(format!("expected {count} keys, found {}", keys.len())),
            None => Some(format!("expected keys; the answer was {outcome:?}")),
        },
        Expectation::Value(written) => {
            let expected = match literal(written) {
                Ok(value) => value,
                Err(reason) => return Some(reason),
            };
            match outcome.value() {
                Some(found) if *found == expected => None,
                Some(found) => Some(format!("expected {expected:?}, found {found:?}")),
                None => Some(format!("expected a value; the answer was {outcome:?}")),
            }
        }
        Expectation::Ok | Expectation::Error(_) => None,
    }
}

/// Read an expected value, written in TessariQL.
///
/// The corpus states what it expects in the language it is testing, so there is
/// one definition of what `dec 12.34` means rather than two that can drift.
fn literal(written: &str) -> Result<Value, String> {
    let script = format!("SET expected:0 = {written};");
    let parsed = parse(&script).map_err(|error| format!("{written:?} is not a value: {error}"))?;
    let Some(StatementKind::Set { value, .. }) =
        parsed.statements.first().map(|statement| &statement.kind)
    else {
        return Err(format!("{written:?} is not a value"));
    };
    value_of(value)
        .ok_or_else(|| format!("{written:?} needs the store to evaluate; an expectation may not"))
}

/// The value an expression denotes, when it needs nothing but itself.
///
/// A read cannot appear here: an expectation that has to consult the store to
/// say what it expects is not an expectation.
fn value_of(expr: &Expr) -> Option<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Some(value.clone()),
        ExprKind::Array(items) => items
            .iter()
            .map(value_of)
            .collect::<Option<_>>()
            .map(Value::Array),
        ExprKind::Set(items) => items
            .iter()
            .map(value_of)
            .collect::<Option<Vec<_>>>()
            .map(|values| Value::Set(values.into_iter().collect())),
        ExprKind::Object(fields) => fields
            .iter()
            .map(|field| value_of(&field.value).map(|value| (field.name.text.clone(), value)))
            .collect::<Option<_>>()
            .map(Value::Object),
        // A conditional and a coalesce are values only once something has
        // decided which side wins, and deciding is evaluation. An expectation
        // that needs evaluating is not an expectation.
        ExprKind::If { .. }
        | ExprKind::Coalesce(..)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Range(_) => None,
        ExprKind::Get(_) | ExprKind::Select(_) => None,
        // A test is not a value, and a path needs a record to read from —
        // neither can stand where a corpus says what it expects. Nor can a
        // parameter: a corpus case supplies no bindings, so an expectation
        // written with one would be saying it expects whatever it was handed.
        ExprKind::Path(_) | ExprKind::Not(_) | ExprKind::Parameter(_) => None,
        // A fold needs a group, and a corpus expectation has none.
        ExprKind::Fold { .. } => None,
        ExprKind::And(_, _) | ExprKind::Or(_, _) | ExprKind::Binary { .. } => None,
        // Arithmetic and a call could be folded here, and are not: an
        // expectation that computes is one that can be wrong in the same way
        // the thing it checks is wrong.
        ExprKind::Negate(_) | ExprKind::Arithmetic { .. } | ExprKind::Call { .. } => None,
    }
}

/// The name a corpus uses for a failure.
///
/// The variant, not the message: a corpus that asserted wording would fail every
/// time a message improved, and would stop being changed.
fn kind_name(error: &Error) -> &'static str {
    match error {
        Error::Script(inner) => script_kind(inner),
        Error::Store(inner) => store_kind(inner),
        // Named for the refusal it carries, not for the wrapper. A corpus row
        // asserts what was refused; whether the session could also say how to
        // fix it is an improvement to the message, which is exactly what this
        // function exists not to be sensitive to.
        Error::UndeclaredField { refusal, .. } => store_kind(refusal),
        Error::Encoding(_) => "Encoding",
        Error::WriteWouldLeaveAHole { .. } => "WriteWouldLeaveAHole",
        Error::FileAboveBucketCeiling { .. } => "FileAboveBucketCeiling",
        Error::NoSuchRouteToAssign { .. } => "NoSuchRouteToAssign",
        Error::NoNamespaceSelected { .. } => "NoNamespaceSelected",
        Error::NoDatabaseSelected { .. } => "NoDatabaseSelected",
        Error::Unknown { .. } => "Unknown",
        Error::NestedTransaction { .. } => "NestedTransaction",
        Error::NoOpenTransaction { .. } => "NoOpenTransaction",
        Error::VersionInsideTransaction { .. } => "VersionInsideTransaction",
        Error::UnclosedTransaction { .. } => "UnclosedTransaction",
        Error::RecordExists { .. } => "RecordExists",
        Error::NoSuchRecord { .. } => "NoSuchRecord",
        Error::InvalidKeyBound { .. } => "InvalidKeyBound",
        Error::NotAnEdgeTable { .. } => "NotAnEdgeTable",
        Error::EndpointsNotDeclared { .. } => "EndpointsNotDeclared",
        Error::EndpointOutsideGraph { .. } => "EndpointOutsideGraph",
        Error::NoHistoricalTraversal { .. } => "NoHistoricalTraversal",
        Error::EdgePropertiesNotAnObject { .. } => "EdgePropertiesNotAnObject",
        Error::ConditionNotBoolean { .. } => "ConditionNotBoolean",
        Error::NoRecordInScope { .. } => "NoRecordInScope",
        Error::DefaultDoesNotMatch { .. } => "DefaultDoesNotMatch",
        Error::NotSummable { .. } => "NotSummable",
        Error::NotSignedIn { .. } => "NotSignedIn",
        Error::RoleForbids { .. } => "RoleForbids",
        Error::SignInRefused => "SignInRefused",
        Error::SignInThrottled => "SignInThrottled",
        Error::NoSuchRole { .. } => "NoSuchRole",
        Error::NoSuchVerb { .. } => "NoSuchVerb",
        Error::NoSuchAuthority { .. } => "NoSuchAuthority",
        Error::CannotHandOut { .. } => "CannotHandOut",
        Error::NotGranted { .. } => "NotGranted",
        Error::GrantedUserCannotDeclare { .. } => "GrantedUserCannotDeclare",
        Error::LastGrant { .. } => "LastGrant",
        Error::NoSearchIndex { .. } => "NoSearchIndex",
        Error::PrefixTooShort { .. } => "PrefixTooShort",
        Error::MalformedSlop { .. } => "MalformedSlop",
        Error::NegationWithoutTerm { .. } => "NegationWithoutTerm",
        Error::NoSuchDistance { .. } => "NoSuchDistance",
        Error::OutsideTenancy { .. } => "OutsideTenancy",
        Error::NotArithmetic { .. } => "NotArithmetic",
        Error::ArithmeticFailed { .. } => "ArithmeticFailed",
        Error::WrongArgument { .. } => "WrongArgument",
        Error::GeometryRefused { .. } => "GeometryRefused",
        Error::CallFailed { .. } => "CallFailed",
        Error::NotCastable { .. } => "NotCastable",
        Error::NotABucket { .. } => "NotABucket",
        Error::NotAQueue { .. } => "NotAQueue",
        Error::ViewIsNotATable { .. } => "ViewIsNotATable",
        Error::ViewsTooDeep { .. } => "ViewsTooDeep",
        Error::ViewUnreadable { .. } => "ViewUnreadable",
        Error::QueueFieldIsTheEngines { .. } => "QueueFieldIsTheEngines",
        Error::ClaimAboveCeiling { .. } => "ClaimAboveCeiling",
        Error::ClaimDeadlineUnreachable { .. } => "ClaimDeadlineUnreachable",
        Error::NotWrittenByHand { .. } => "NotWrittenByHand",
        Error::FileNeedsAPath { .. } => "FileNeedsAPath",
        Error::FileIsNotBytes { .. } => "FileIsNotBytes",
        Error::FileIsIncomplete { .. } => "FileIsIncomplete",
        Error::NotWritable { .. } => "NotWritable",
        Error::ManyWritablePeers { .. } => "ManyWritablePeers",
        Error::DuplicateMapping { .. } => "DuplicateMapping",
        Error::MergeIsNotAnObject { .. } => "MergeIsNotAnObject",
        Error::Thrown { .. } => "Thrown",
        Error::JoinKeysDiffer { .. } => "JoinKeysDiffer",
        Error::NoSuchAccessPath { .. } => "NoSuchAccessPath",
        Error::PathNotTaken { .. } => "PathNotTaken",
        Error::IndexNotUsed { .. } => "IndexNotUsed",
        Error::TimedOut { .. } => "TimedOut",
        Error::Unbounded { .. } => "Unbounded",
        Error::UnboundedCollection { .. } => "UnboundedCollection",
        Error::NotAlone { .. } => "NotAlone",
        Error::AnchorGone { .. } => "AnchorGone",
        Error::StillDepended { .. } => "StillDepended",
        Error::TableBelongsToGraph { .. } => "TableBelongsToGraph",
        Error::NotReadBySelect { .. } => "NotReadBySelect",
        Error::NotIndexable { .. } => "NotIndexable",
        Error::SecretNeedsVault { .. } => "SecretNeedsVault",
        Error::NotASecret { .. } => "NotASecret",
        Error::VaultIsStrict { .. } => "VaultIsStrict",
        Error::VaultEditComputesFromTheRecord { .. } => "VaultEditComputesFromTheRecord",
        Error::RecipientIsNotAName { .. } => "RecipientIsNotAName",
        _ => "Unnamed",
    }
}

fn script_kind(error: &tessari_ql::Error) -> &'static str {
    match error {
        tessari_ql::Error::EmptyTimeout { .. } => "EmptyTimeout",
        tessari_ql::Error::UnexpectedCharacter { .. } => "UnexpectedCharacter",
        tessari_ql::Error::UnterminatedString { .. } => "UnterminatedString",
        tessari_ql::Error::InvalidEscape { .. } => "InvalidEscape",
        tessari_ql::Error::InvalidNumber { .. } => "InvalidNumber",
        tessari_ql::Error::InvalidBytes { .. } => "InvalidBytes",
        tessari_ql::Error::InvalidDuration { .. } => "InvalidDuration",
        tessari_ql::Error::UnexpectedToken { .. } => "UnexpectedToken",
        tessari_ql::Error::UnexpectedEnd { .. } => "UnexpectedEnd",
        tessari_ql::Error::Unsupported { .. } => "Unsupported",
        tessari_ql::Error::InvalidDatetime { .. } => "InvalidDatetime",
        tessari_ql::Error::InvalidUuid { .. } => "InvalidUuid",
        tessari_ql::Error::InvalidDecimal { .. } => "InvalidDecimal",
        tessari_ql::Error::InvalidRecordId { .. } => "InvalidRecordId",
        tessari_ql::Error::NotARange { .. } => "NotARange",
        tessari_ql::Error::DuplicateField { .. } => "DuplicateField",
        tessari_ql::Error::DuplicateProjection { .. } => "DuplicateProjection",
        tessari_ql::Error::UnnamedProjection { .. } => "UnnamedProjection",
        tessari_ql::Error::NotASideOfTheJoin { .. } => "NotASideOfTheJoin",
        tessari_ql::Error::OneSidedJoin { .. } => "OneSidedJoin",
        tessari_ql::Error::JoinKeyIsNotAField { .. } => "JoinKeyIsNotAField",
        tessari_ql::Error::NoSuchFunction { .. } => "NoSuchFunction",
        tessari_ql::Error::WrongArity { .. } => "WrongArity",
        tessari_ql::Error::UngroupedProjection { .. } => "UngroupedProjection",
        tessari_ql::Error::StarIsOnlyForCount { .. } => "StarIsOnlyForCount",
        tessari_ql::Error::SeveralOutsideAComparison { .. } => "SeveralOutsideAComparison",
        tessari_ql::Error::AssertionNotAConstraint { .. } => "AssertionNotAConstraint",
        tessari_ql::Error::SeveralInAUniqueIndex { .. } => "SeveralInAUniqueIndex",
        tessari_ql::Error::SeveralInAnAnalysedIndex { .. } => "SeveralInAnAnalysedIndex",
        tessari_ql::Error::SeveralRoutesInOneIndex { .. } => "SeveralRoutesInOneIndex",
        tessari_ql::Error::FoldInsideAFold { .. } => "FoldInsideAFold",
        tessari_ql::Error::FoldInAFilter { .. } => "FoldInAFilter",
        tessari_ql::Error::MalformedGeometry { .. } => "MalformedGeometry",
        tessari_ql::Error::ComputedGeometry { .. } => "ComputedGeometry",
        tessari_ql::Error::UnboundParameter { .. } => "UnboundParameter",
        tessari_ql::Error::BoundTwice { .. } => "BoundTwice",
        tessari_ql::Error::BindingCollidesWithParameter { .. } => "BindingCollidesWithParameter",
        tessari_ql::Error::ReturnedTwice { .. } => "ReturnedTwice",
        tessari_ql::Error::CursorBesideAnOffset { .. } => "CursorBesideAnOffset",
        tessari_ql::Error::CursorBesideAReshaping { .. } => "CursorBesideAReshaping",
        tessari_ql::Error::AnchorFromAnotherTable { .. } => "AnchorFromAnotherTable",
        tessari_ql::Error::InsertRowArity { .. } => "InsertRowArity",
        tessari_ql::Error::TableWithoutColumns { .. } => "TableWithoutColumns",
        tessari_ql::Error::DepthNeedsOneHopToATable { .. } => "DepthNeedsOneHopToATable",
        tessari_ql::Error::DepthBelowOne { .. } => "DepthBelowOne",
        tessari_ql::Error::VectorWidthBelowOne { .. } => "VectorWidthBelowOne",
        tessari_ql::Error::VectorWidthAboveTheCeiling { .. } => "VectorWidthAboveTheCeiling",
        tessari_ql::Error::EffortBelowOne { .. } => "EffortBelowOne",
        _ => "Unnamed",
    }
}

fn store_kind(error: &tessari_storage::Error) -> &'static str {
    match error {
        tessari_storage::Error::Conflict { .. } => "Conflict",
        tessari_storage::Error::CommitContention { .. } => "CommitContention",
        tessari_storage::Error::NameTaken { .. } => "NameTaken",
        tessari_storage::Error::EmptyIndex { .. } => "EmptyIndex",
        tessari_storage::Error::UniqueViolation { .. } => "UniqueViolation",
        tessari_storage::Error::SchemaViolation { .. } => "SchemaViolation",
        tessari_storage::Error::UndeclaredField { .. } => "UndeclaredField",
        tessari_storage::Error::RecordsRefused { .. } => "RecordsRefused",
        tessari_storage::Error::MissingRequiredField { .. } => "MissingRequiredField",
        tessari_storage::Error::AssertionViolation { .. } => "AssertionViolation",
        tessari_storage::Error::NoSuchParent { .. } => "NoSuchParent",
        tessari_storage::Error::VersionReclaimed { .. } => "VersionReclaimed",
        tessari_storage::Error::VersionInTheFuture { .. } => "VersionInTheFuture",
        // One name for every refusal the vault crate raises, deliberately. The
        // distinctions it draws — sealed, wrong key, unknown algorithm — matter
        // to an operator and must not become a corpus vocabulary a client can
        // branch on: "which of these went wrong" is exactly the question an
        // attacker asks, and a stable name per variant is an answer.
        tessari_storage::Error::Vault(_) => "Vault",
        tessari_storage::Error::VaultUnavailable => "VaultUnavailable",
        tessari_storage::Error::VaultReservedField { .. } => "VaultReservedField",
        tessari_storage::Error::VaultNotAnObject { .. } => "VaultNotAnObject",
        tessari_storage::Error::VaultNoKey { .. } => "VaultNoKey",
        tessari_storage::Error::VaultReservedRecipient { .. } => "VaultReservedRecipient",
        tessari_storage::Error::VaultRecipientExists { .. } => "VaultRecipientExists",
        tessari_storage::Error::VaultNoRecipient { .. } => "VaultNoRecipient",
        _ => "Unnamed",
    }
}
