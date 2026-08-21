//! Asking whether text holds a word, and where the answer comes from.
//!
//! The analyzer is read from the **schema**, not from an index, which is the
//! whole of why full-text search here cannot behave differently once an index
//! exists. See [`bgv_db_types::Analyzer`] for the argument; this module is where
//! that decision is spent.

use std::collections::BTreeMap;

use bgv_db_ql::{BinaryOp, Expr, ExprKind};
use bgv_db_storage::{Catalog, Transaction};
use bgv_db_types::{Analyzer, Path, TableId, Value};

use crate::error::Result;
use crate::session::Session;

impl Session<'_> {
    /// The analyzer each searched path carries, read from the schema once.
    ///
    /// A `MATCHES` whose field declares no analyzer finds nothing, rather than
    /// failing: a schemaless table is allowed to hold text nobody has declared
    /// anything about, and refusing the query would make that a mistake.
    pub(crate) fn analyzers_for(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        condition: &Expr,
    ) -> Result<BTreeMap<Path, Analyzer>> {
        let mut wanted = Vec::new();
        searched_paths(condition, &mut wanted);
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }
        let declared = Catalog::new(transaction).fields_on(table)?;
        let mut named = BTreeMap::new();
        for field in declared {
            let Some(analyzer) = field.analyzer else {
                continue;
            };
            if !wanted.iter().any(|path| path.root() == field.name) {
                continue;
            }
            named.insert(field.name, analyzer);
        }
        let mut resolved = BTreeMap::new();
        for definition in Catalog::new(transaction).analyzers()? {
            for path in &wanted {
                if named.get(path.root()) == Some(&definition.name) {
                    resolved.insert((*path).clone(), definition.analyzer.clone());
                }
            }
        }
        Ok(resolved)
    }
}

/// The paths a condition searches with `MATCHES`.
fn searched_paths<'a>(condition: &'a Expr, into: &mut Vec<&'a Path>) {
    match &condition.kind {
        ExprKind::Binary {
            op: BinaryOp::Matches,
            left,
            ..
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
            }
        }
        ExprKind::And(left, right) | ExprKind::Or(left, right) => {
            searched_paths(left, into);
            searched_paths(right, into);
        }
        ExprKind::Not(inner) => searched_paths(inner, into),
        _ => {}
    }
}

/// Whether the analyzed text holds every term of the query.
///
/// **Every** term, because "find me documents about X Y" means both — and a
/// field with no analyzer holds no terms, so it matches nothing rather than
/// failing.
pub(crate) fn matches_terms(analyzer: Option<&Analyzer>, held: &Value, wanted: &Value) -> bool {
    let (Some(analyzer), Value::String(text), Value::String(query)) = (analyzer, held, wanted)
    else {
        return false;
    };
    let terms = analyzer.terms(text);
    let asked = analyzer.terms(query);
    !asked.is_empty() && asked.iter().all(|term| terms.contains(term))
}
