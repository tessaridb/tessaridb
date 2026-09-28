//! Running a corpus, and saying precisely what failed.

mod kinds;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::{Expr, ExprKind, StatementKind, parse};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

use crate::case::{Case, Corpus, Expectation};
pub(crate) use kinds::kind_name;

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
    run_on(corpus, Arc::new(MemoryBackend::new()))
}

/// Run every case of a corpus against one fresh store on `backend`.
///
/// For a corpus whose promise is that it holds on every backend — the
/// key-value one is run on the disk backend as well (G035) — so a behaviour
/// that only the memory backend happens to give cannot pass as the language's.
#[must_use]
pub fn run_on(corpus: &Corpus, backend: Arc<dyn KvBackend>) -> Vec<CaseResult> {
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
        ExprKind::Get(_) | ExprKind::Ttl(_) | ExprKind::Select(_) => None,
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
