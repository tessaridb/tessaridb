//! What one **read** needs from the schema and the collection, resolved before
//! any record exists.
//!
//! This is the once-per-read half of the module: the analyzer each searched
//! field declares, the collection's numbers behind each ranked one, and the
//! contract checks that must not depend on which access path the planner later
//! picks.

use std::collections::BTreeMap;

use tessari_ql::{BinaryOp, Expr, ExprKind, Function};
use tessari_storage::{Catalog, IndexDefinition, Transaction};
use tessari_types::{Analyzer, Path, TableId, Value};

use tessari_constants::{SEARCH_FUZZY_PREFIX, SEARCH_PREFIX_MINIMUM};

use crate::error::{Error, Result};
use crate::rank::Corpus;
use crate::session::Session;

use super::query::malformed_slop;

/// What one ranked path was resolved against.
///
/// The collection's numbers and the index they were read from, kept together
/// because they are established at the same moment and used at the same one: the
/// index is where the *record's* two numbers come from, once there is a record.
#[derive(Debug, Clone)]
pub(crate) struct Ranked {
    /// What the collection looks like.
    pub(crate) corpus: Corpus,
    /// The search index this path's statistics were read from.
    pub(crate) index: IndexDefinition,
}

/// What the searched fields of one read need, resolved before any record is.
#[derive(Debug, Clone, Default)]
pub(crate) struct Searched {
    analyzers: BTreeMap<Path, Analyzer>,
    corpora: BTreeMap<Path, Ranked>,
}

impl Searched {
    /// The analyzer this path's field declares, if it declares one.
    pub(crate) fn analyzer(&self, path: &Path) -> Option<&Analyzer> {
        self.analyzers.get(path)
    }

    /// What this path was ranked against, if it was ranked at all.
    pub(crate) fn ranked(&self, path: &Path) -> Option<&Ranked> {
        self.corpora.get(path)
    }
}

impl Session<'_> {
    /// Everything the searched fields of these expressions need.
    ///
    /// A `MATCHES` whose field declares no analyzer finds nothing, rather than
    /// failing: a schemaless table is allowed to hold text nobody has declared
    /// anything about, and refusing the query would make that a mistake.
    ///
    /// A `search::score` whose field has no analyzer *or* no search index gets
    /// no corpus, and is refused when it is evaluated — for the reason
    /// [`crate::rank`] sets out at length.
    pub(crate) fn searched_for(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        expressions: &[&Expr],
    ) -> Result<Searched> {
        let mut wanted = Vec::new();
        let mut ranked: Vec<(&Path, &Expr)> = Vec::new();
        let mut prefixed: Vec<(&Path, &Expr, BinaryOp)> = Vec::new();
        let mut phrased: Vec<&Expr> = Vec::new();
        for expr in expressions {
            searched_paths(expr, &mut wanted, &mut ranked, &mut prefixed, &mut phrased);
        }

        // The phrase contract is checked FIRST, before the catalog is read at
        // all — earlier even than the prefix contract below, which needs an
        // analyzer. A malformed slop marker is a mistake in the query and not a
        // question about the data, so nothing about the table, the field or the
        // indexes may change whether it is refused.
        for query in phrased {
            let Value::String(text) = self.evaluate(transaction, query)? else {
                continue;
            };
            if let Some(marker) = malformed_slop(&text) {
                return Err(Error::MalformedSlop {
                    marker: marker.to_owned(),
                    span: query.span,
                });
            }
        }

        if wanted.is_empty() {
            return Ok(Searched::default());
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
        let mut analyzers = BTreeMap::new();
        for definition in Catalog::new(transaction).analyzers()? {
            for path in &wanted {
                if named.get(path.root()) == Some(&definition.name) {
                    analyzers.insert((*path).clone(), definition.analyzer.clone());
                }
            }
        }

        // The prefix contract is checked **here**, once per read and before any
        // access path exists. That placement is the whole guarantee: a refusal
        // raised where the index is chosen would make the same statement run on
        // a table without an index and fail on one with it, which is the exact
        // failure this store's access-path rule exists to prevent.
        for (path, query, op) in prefixed {
            let Some(analyzer) = analyzers.get(path) else {
                continue;
            };
            let Value::String(text) = self.evaluate(transaction, query)? else {
                continue;
            };
            // Both operators bound the *front* of a typed word, and both are
            // refused on the same footing — but the limits are different numbers
            // for different reasons, so which one applies is decided here rather
            // than by one shared constant standing in for two contracts.
            let minimum = match op {
                BinaryOp::MatchesFuzzy => SEARCH_FUZZY_PREFIX,
                _ => SEARCH_PREFIX_MINIMUM,
            };
            for alternatives in analyzer.prefixes(&text) {
                // The **typed** spelling decides, which is the first of the
                // alternatives. Judging by the stem instead would refuse
                // `runs` — three letters typed, two stored — and admit a word
                // whose stem happens to be long, so the limit would depend on
                // English morphology rather than on what the reader wrote.
                let Some(prefix) = alternatives.first() else {
                    continue;
                };
                if prefix.chars().count() < minimum {
                    return Err(Error::PrefixTooShort {
                        prefix: prefix.clone(),
                        minimum,
                        span: query.span,
                    });
                }
            }
        }

        let mut corpora = BTreeMap::new();
        for (path, query) in ranked {
            let Some(analyzer) = analyzers.get(path) else {
                continue;
            };
            let Some(index) = self.index_on_path(transaction, table, path)? else {
                continue;
            };
            if !index.search {
                continue;
            }
            // Only the terms this statement asks about: counting the rest would
            // be reading the index to answer a question nobody put.
            let asked = match self.evaluate(transaction, query)? {
                Value::String(text) => analyzer.terms(&text),
                _ => Vec::new(),
            };
            let statistics = transaction.search_statistics(&index)?;
            let mut frequencies = BTreeMap::new();
            for term in &asked {
                if frequencies.contains_key(term) {
                    continue;
                }
                let held = transaction.document_frequency(&index, term)?;
                frequencies.insert(term.clone(), held);
            }
            corpora.insert(
                path.clone(),
                Ranked {
                    corpus: Corpus {
                        documents: statistics.documents,
                        average_length: statistics.average_length().unwrap_or_default(),
                        frequencies,
                        asked,
                    },
                    index,
                },
            );
        }

        Ok(Searched { analyzers, corpora })
    }
}

/// The paths a set of expressions searches, and the ones it ranks by.
///
/// A ranked path is also a searched one — it needs the analyzer as well — so it
/// is recorded in both places rather than the caller having to remember that.
fn searched_paths<'a>(
    expr: &'a Expr,
    into: &mut Vec<&'a Path>,
    ranked: &mut Vec<(&'a Path, &'a Expr)>,
    prefixed: &mut Vec<(&'a Path, &'a Expr, BinaryOp)>,
    phrased: &mut Vec<&'a Expr>,
) {
    match &expr.kind {
        ExprKind::Binary {
            op: BinaryOp::Matches,
            left,
            right,
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
                phrased.push(right);
            }
        }
        ExprKind::Binary {
            op: op @ (BinaryOp::MatchesPrefix | BinaryOp::MatchesFuzzy),
            left,
            right,
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
                prefixed.push((&field.path, right, *op));
            }
        }
        ExprKind::Call {
            function: Function::SearchScore,
            arguments,
            ..
        } => {
            let (Some(first), Some(second)) = (arguments.first(), arguments.get(1)) else {
                return;
            };
            if let ExprKind::Path(field) = &first.kind {
                into.push(&field.path);
                ranked.push((&field.path, second));
            }
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                searched_paths(argument, into, ranked, prefixed, phrased);
            }
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Binary { left, right, .. }
        | ExprKind::Arithmetic { left, right, .. } => {
            searched_paths(left, into, ranked, prefixed, phrased);
            searched_paths(right, into, ranked, prefixed, phrased);
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => {
            searched_paths(inner, into, ranked, prefixed, phrased);
        }
        _ => {}
    }
}
