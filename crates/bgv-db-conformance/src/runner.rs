//! Running a corpus, and saying precisely what failed.

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_ql::{Expr, ExprKind, StatementKind, parse};
use bgv_db_session::{Error, Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::Value;

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

/// Read an expected value, written in bgvQL.
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
        ExprKind::Table(_) | ExprKind::Record(_) | ExprKind::Range(_) => None,
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
        Error::Encoding(_) => "Encoding",
        Error::NoNamespaceSelected { .. } => "NoNamespaceSelected",
        Error::NoDatabaseSelected { .. } => "NoDatabaseSelected",
        Error::Unknown { .. } => "Unknown",
        Error::NestedTransaction { .. } => "NestedTransaction",
        Error::NoOpenTransaction { .. } => "NoOpenTransaction",
        Error::UnclosedTransaction { .. } => "UnclosedTransaction",
        Error::RecordExists { .. } => "RecordExists",
        Error::NoSuchRecord { .. } => "NoSuchRecord",
        Error::InvalidKeyBound { .. } => "InvalidKeyBound",
        Error::NotAnEdgeTable { .. } => "NotAnEdgeTable",
        Error::EdgePropertiesNotAnObject { .. } => "EdgePropertiesNotAnObject",
        Error::ConditionNotBoolean { .. } => "ConditionNotBoolean",
        Error::NoRecordInScope { .. } => "NoRecordInScope",
        Error::DefaultDoesNotMatch { .. } => "DefaultDoesNotMatch",
        Error::NotSummable { .. } => "NotSummable",
        Error::NotSignedIn { .. } => "NotSignedIn",
        Error::RoleForbids { .. } => "RoleForbids",
        Error::SignInRefused => "SignInRefused",
        Error::NoSuchRole { .. } => "NoSuchRole",
        Error::NoSuchVerb { .. } => "NoSuchVerb",
        Error::NotGranted { .. } => "NotGranted",
        Error::GrantedUserCannotDeclare { .. } => "GrantedUserCannotDeclare",
        Error::LastGrant { .. } => "LastGrant",
        Error::NoSearchIndex { .. } => "NoSearchIndex",
        Error::NoSuchDistance { .. } => "NoSuchDistance",
        Error::OutsideTenancy { .. } => "OutsideTenancy",
        Error::NotArithmetic { .. } => "NotArithmetic",
        Error::ArithmeticFailed { .. } => "ArithmeticFailed",
        Error::WrongArgument { .. } => "WrongArgument",
        Error::CallFailed { .. } => "CallFailed",
        Error::NotABucket { .. } => "NotABucket",
        Error::NotWrittenByHand { .. } => "NotWrittenByHand",
        Error::FileNeedsAPath { .. } => "FileNeedsAPath",
        Error::FileIsNotBytes { .. } => "FileIsNotBytes",
        Error::FileIsIncomplete { .. } => "FileIsIncomplete",
        _ => "Unnamed",
    }
}

fn script_kind(error: &bgv_db_ql::Error) -> &'static str {
    match error {
        bgv_db_ql::Error::UnexpectedCharacter { .. } => "UnexpectedCharacter",
        bgv_db_ql::Error::UnterminatedString { .. } => "UnterminatedString",
        bgv_db_ql::Error::InvalidEscape { .. } => "InvalidEscape",
        bgv_db_ql::Error::InvalidNumber { .. } => "InvalidNumber",
        bgv_db_ql::Error::InvalidBytes { .. } => "InvalidBytes",
        bgv_db_ql::Error::InvalidDuration { .. } => "InvalidDuration",
        bgv_db_ql::Error::UnexpectedToken { .. } => "UnexpectedToken",
        bgv_db_ql::Error::UnexpectedEnd { .. } => "UnexpectedEnd",
        bgv_db_ql::Error::Unsupported { .. } => "Unsupported",
        bgv_db_ql::Error::InvalidDatetime { .. } => "InvalidDatetime",
        bgv_db_ql::Error::InvalidUuid { .. } => "InvalidUuid",
        bgv_db_ql::Error::InvalidDecimal { .. } => "InvalidDecimal",
        bgv_db_ql::Error::InvalidRecordId { .. } => "InvalidRecordId",
        bgv_db_ql::Error::NotARange { .. } => "NotARange",
        bgv_db_ql::Error::DuplicateField { .. } => "DuplicateField",
        bgv_db_ql::Error::DuplicateProjection { .. } => "DuplicateProjection",
        bgv_db_ql::Error::UnnamedProjection { .. } => "UnnamedProjection",
        bgv_db_ql::Error::NotASideOfTheJoin { .. } => "NotASideOfTheJoin",
        bgv_db_ql::Error::OneSidedJoin { .. } => "OneSidedJoin",
        bgv_db_ql::Error::JoinKeyIsNotAField { .. } => "JoinKeyIsNotAField",
        bgv_db_ql::Error::NoSuchFunction { .. } => "NoSuchFunction",
        bgv_db_ql::Error::WrongArity { .. } => "WrongArity",
        bgv_db_ql::Error::UngroupedProjection { .. } => "UngroupedProjection",
        bgv_db_ql::Error::StarIsOnlyForCount { .. } => "StarIsOnlyForCount",
        bgv_db_ql::Error::SeveralOutsideAComparison { .. } => "SeveralOutsideAComparison",
        bgv_db_ql::Error::SeveralInAUniqueIndex { .. } => "SeveralInAUniqueIndex",
        bgv_db_ql::Error::SeveralInAnAnalysedIndex { .. } => "SeveralInAnAnalysedIndex",
        bgv_db_ql::Error::SeveralRoutesInOneIndex { .. } => "SeveralRoutesInOneIndex",
        bgv_db_ql::Error::FoldInsideAFold { .. } => "FoldInsideAFold",
        bgv_db_ql::Error::FoldInAFilter { .. } => "FoldInAFilter",
        _ => "Unnamed",
    }
}

fn store_kind(error: &bgv_db_storage::Error) -> &'static str {
    match error {
        bgv_db_storage::Error::Conflict { .. } => "Conflict",
        bgv_db_storage::Error::CommitContention { .. } => "CommitContention",
        bgv_db_storage::Error::NameTaken { .. } => "NameTaken",
        bgv_db_storage::Error::EmptyIndex { .. } => "EmptyIndex",
        bgv_db_storage::Error::UniqueViolation { .. } => "UniqueViolation",
        bgv_db_storage::Error::SchemaViolation { .. } => "SchemaViolation",
        bgv_db_storage::Error::UndeclaredField { .. } => "UndeclaredField",
        bgv_db_storage::Error::MissingRequiredField { .. } => "MissingRequiredField",
        bgv_db_storage::Error::NoSuchParent { .. } => "NoSuchParent",
        _ => "Unnamed",
    }
}
