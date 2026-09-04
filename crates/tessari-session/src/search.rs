//! Asking whether text holds a word, how much that is worth, and where each
//! answer comes from.
//!
//! The analyzer is read from the **schema**, not from an index, which is the
//! whole of why full-text search here cannot behave differently once an index
//! exists. See [`tessari_types::Analyzer`] for the argument; this module is where
//! that decision is spent.
//!
//! # Two questions, resolved together and answered differently
//!
//! `MATCHES` asks about one document and needs only the analyzer. `search::score`
//! asks about a document *relative to a collection* and additionally needs what
//! the collection looks like — how many documents there are, how long a typical
//! one is, and how many hold each of the query's words. See [`crate::rank`] for
//! why that difference decides whether an index is optional or required.
//!
//! Both are resolved **once per read** rather than once per record, because the
//! schema does not change under a read and neither does the collection. They
//! travel together in [`Searched`] because they are needed in the same places
//! and have the same lifetime: a sort key is an expression too, and a `MATCHES`
//! or a score in one must mean what it means in the `WHERE` that produced the
//! records.

use std::collections::BTreeMap;

use tessari_ql::{BinaryOp, Expr, ExprKind, Function};
use tessari_storage::{Catalog, Transaction};
use tessari_types::{Analyzer, Path, TableId, Value};

use tessari_constants::SEARCH_PREFIX_MINIMUM;

use crate::error::{Error, Result};
use crate::rank::Corpus;
use crate::session::Session;

/// What the searched fields of one read need, resolved before any record is.
#[derive(Debug, Clone, Default)]
pub(crate) struct Searched {
    analyzers: BTreeMap<Path, Analyzer>,
    corpora: BTreeMap<Path, Corpus>,
}

impl Searched {
    /// The analyzer this path's field declares, if it declares one.
    pub(crate) fn analyzer(&self, path: &Path) -> Option<&Analyzer> {
        self.analyzers.get(path)
    }

    /// What this path's collection looks like, if it was ranked against.
    pub(crate) fn corpus(&self, path: &Path) -> Option<&Corpus> {
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
        let mut prefixed: Vec<(&Path, &Expr)> = Vec::new();
        for expr in expressions {
            searched_paths(expr, &mut wanted, &mut ranked, &mut prefixed);
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
        for (path, query) in prefixed {
            let Some(analyzer) = analyzers.get(path) else {
                continue;
            };
            let Value::String(text) = self.evaluate(transaction, query)? else {
                continue;
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
                if prefix.chars().count() < SEARCH_PREFIX_MINIMUM {
                    return Err(Error::PrefixTooShort {
                        prefix: prefix.clone(),
                        minimum: SEARCH_PREFIX_MINIMUM,
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
            for term in asked {
                if frequencies.contains_key(&term) {
                    continue;
                }
                let held = transaction.document_frequency(&index, &term)?;
                frequencies.insert(term, held);
            }
            corpora.insert(
                path.clone(),
                Corpus {
                    documents: statistics.documents,
                    average_length: statistics.average_length().unwrap_or_default(),
                    frequencies,
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
    prefixed: &mut Vec<(&'a Path, &'a Expr)>,
) {
    match &expr.kind {
        ExprKind::Binary {
            op: BinaryOp::Matches,
            left,
            ..
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
            }
        }
        ExprKind::Binary {
            op: BinaryOp::MatchesPrefix,
            left,
            right,
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
                prefixed.push((&field.path, right));
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
                searched_paths(argument, into, ranked, prefixed);
            }
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Binary { left, right, .. }
        | ExprKind::Arithmetic { left, right, .. } => {
            searched_paths(left, into, ranked, prefixed);
            searched_paths(right, into, ranked, prefixed);
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => {
            searched_paths(inner, into, ranked, prefixed);
        }
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

/// Whether the analyzed text holds a term beginning with every prefix of the
/// query.
///
/// **Every** prefix and **a** term: the conjunction is across what was typed and
/// the disjunction is within each word, which is what "documents about vecto…
/// and lo…" means. The same shape `MATCHES` has, one level looser.
///
/// The query is analysed with [`Analyzer::prefixes`] rather than
/// [`Analyzer::terms`] — the beginning of a word is not a word, and stemming it
/// produces the beginning of nothing. That function carries the argument.
///
/// This is the **scan**'s answer. The index reaches the same set through the term
/// dictionary, and the two are asserted to agree record for record, exactly as
/// they are for `MATCHES`.
pub(crate) fn matches_prefix_terms(
    analyzer: Option<&Analyzer>,
    held: &Value,
    wanted: &Value,
) -> bool {
    let (Some(analyzer), Value::String(text), Value::String(query)) = (analyzer, held, wanted)
    else {
        return false;
    };
    let terms = analyzer.terms(text);
    let asked = analyzer.prefixes(query);
    !asked.is_empty()
        && asked.iter().all(|alternatives| {
            alternatives
                .iter()
                .any(|prefix| terms.iter().any(|term| term.starts_with(prefix)))
        })
}
