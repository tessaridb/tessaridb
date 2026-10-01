//! What one **read** needs from the schema and the collection, resolved before
//! any record exists.
//!
//! This is the once-per-read half of the module: the analyzer each searched
//! field declares, the collection's numbers behind each ranked one, and the
//! contract checks that must not depend on which access path the planner later
//! picks.

use std::collections::BTreeMap;

use tessari_encoding::{SearchStatistics, TermStatistics};
use tessari_ql::{BinaryOp, Expr, ExprKind, Function, Span};
use tessari_storage::{Catalog, IndexDefinition, Transaction};
use tessari_types::{Analyzer, Path, TableId, Value};

use tessari_constants::{
    SEARCH_FUZZY_PREFIX, SEARCH_PREFIX_EXPANSION_CAP, SEARCH_PREFIX_MINIMUM,
    SEARCH_PREFIX_SCORE_EXAMINATION_CAP,
};

use crate::error::{Error, Result};
use crate::evaluate::Part;
use crate::outcome::Suggestion;
use crate::rank::{Blend, Corpus};
use crate::session::Session;

use super::query::{Word, asked, malformed_slop, negation_without_term, scored_words};
use super::suggest::suggested;

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
    /// Whether a record's own text is what it is scored by, rather than this
    /// node's postings: the read gathers records this node's index does not
    /// hold (ADR-0103 D2).
    pub(crate) from_text: bool,
}

/// What the searched fields of one read need, resolved before any record is.
#[derive(Debug, Clone, Default)]
pub(crate) struct Searched {
    analyzers: BTreeMap<Path, Analyzer>,
    corpora: BTreeMap<Path, Ranked>,
    wanted: BTreeMap<Path, Vec<(BinaryOp, String)>>,
    suggestion: Option<Suggestion>,
}

impl Searched {
    /// The analyzer this path's field declares, if it declares one.
    pub(crate) fn analyzer(&self, path: &Path) -> Option<&Analyzer> {
        self.analyzers.get(path)
    }

    /// What this read asked of this path — every `MATCHES`, `MATCHES PREFIX` and
    /// `MATCHES FUZZY` naming it, with the query text each was given.
    ///
    /// Recorded here because this is where the query expressions are evaluated,
    /// and evaluated **once**: a highlight computed from a second evaluation
    /// could mark a record against a query the filter never saw.
    pub(crate) fn wanted(&self, path: &Path) -> &[(BinaryOp, String)] {
        self.wanted.get(path).map_or(&[], Vec::as_slice)
    }

    /// What this path was ranked against, if it was ranked at all.
    pub(crate) fn ranked(&self, path: &Path) -> Option<&Ranked> {
        self.corpora.get(path)
    }

    /// What the query might have meant, resolved here because this is the one
    /// place a read has both the analyzer and the dictionary in hand — and
    /// resolved *before* any access path is chosen, so that a suggestion cannot
    /// come to depend on which candidate the planner picked.
    pub(crate) fn suggestion(&self) -> Option<Suggestion> {
        self.suggestion.clone()
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
        let mut phrased: Vec<(&Path, &Expr)> = Vec::new();
        for expr in expressions {
            searched_paths(expr, &mut wanted, &mut ranked, &mut prefixed, &mut phrased);
        }

        // The query contracts are checked FIRST, before the catalog is read at
        // all — earlier even than the prefix contract below, which needs an
        // analyzer. A malformed slop marker and a query that excludes without
        // requiring are both mistakes in the query rather than questions about
        // the data, so nothing about the table, the field or the indexes may
        // change whether they are refused.
        //
        // The evaluated text is KEPT rather than dropped, because the suggestion
        // at the end of this function asks about the same strings. Evaluating
        // them a second time would let a query built from an expression be
        // checked as one thing and suggested against as another.
        let mut asked_of: BTreeMap<Path, Vec<(BinaryOp, String)>> = BTreeMap::new();
        let mut matched: Vec<(Path, String)> = Vec::with_capacity(phrased.len());
        let mut matched_at: Vec<Span> = Vec::with_capacity(phrased.len());
        for (path, query) in phrased {
            let Value::String(text) = self.evaluate(transaction, query)? else {
                continue;
            };
            if let Some(marker) = malformed_slop(&text) {
                return Err(Error::MalformedSlop {
                    marker: marker.to_owned(),
                    span: query.span,
                });
            }
            if negation_without_term(&text) {
                return Err(Error::NegationWithoutTerm { span: query.span });
            }
            matched.push((path.clone(), text));
            matched_at.push(query.span);
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
            asked_of
                .entry(path.clone())
                .or_default()
                .push((op, text.clone()));
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

        // A starred word is a prefix and carries the prefix floor, checked here
        // for the same reason as the loop above (ADR-0104 D2).
        for ((path, text), span) in matched.iter().zip(&matched_at) {
            if let Some(analyzer) = analyzers.get(path) {
                too_short(&asked(analyzer, text).prefixes(), *span)?;
            }
        }

        // What each field was asked, kept so a highlight marks against the
        // query this read actually ran rather than a copy of it. Built from the
        // texts the two loops above already evaluated — the prefix loop records
        // as it goes, because it is the only place that knows which of the two
        // operators a query carried.
        for (path, text) in &matched {
            asked_of
                .entry(path.clone())
                .or_default()
                .push((BinaryOp::Matches, text.clone()));
        }

        // What this session may read of the table, asked once per read. A score
        // and a suggestion are both read from the index by identity or by term,
        // and neither ever touches the record the grant redacted — so the field
        // the grant hides has to be hidden here as well, or a caller who cannot
        // read it could rank the table by it and be told the words it holds.
        let visible = self.visible_in(transaction, table)?;
        let hidden = |path: &Path| {
            visible
                .as_ref()
                .is_some_and(|fields| !fields.contains(path.root()))
        };

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
            // A hidden field ranks as a field the record does not hold: every
            // record scores `0`, which is what the redacted record would earn by
            // the missing-field rule. Not a refusal — "this field has no search
            // index" would be false, and a refusal of its own would say the
            // field is there. No statistic of the collection is read either.
            if hidden(path) {
                corpora.insert(
                    path.clone(),
                    Ranked {
                        corpus: Corpus {
                            documents: 0,
                            average_length: 0.0,
                            terms: BTreeMap::new(),
                            asked: Vec::new(),
                            blends: Vec::new(),
                        },
                        index,
                        from_text: false,
                    },
                );
                continue;
            }
            // Only the terms this statement asks about: counting the rest would
            // be reading the index to answer a question nobody put.
            let words = match self.evaluate(transaction, query)? {
                Value::String(text) => scored_words(analyzer, &text),
                _ => Vec::new(),
            };
            let mut asked = Vec::with_capacity(words.len());
            let mut starred = Vec::new();
            for word in words {
                match word {
                    Word::Term(term) => asked.push(term),
                    Word::Prefix(alternatives) => starred.push(alternatives),
                }
            }
            let floors: Vec<&[String]> = starred.iter().map(Vec::as_slice).collect();
            too_short(&floors, query.span)?;
            // ADR-0104 D5: which terms a prefix blends is a question about the
            // whole collection's dictionary, and this node may hold part of it.
            if !starred.is_empty() {
                self.refuse_reading_a_part(transaction, table, Part::Whole)?;
            }
            let statistics = transaction.search_statistics(&index)?;
            let mut terms = BTreeMap::new();
            for term in &asked {
                if terms.contains_key(term) {
                    continue;
                }
                let held = transaction.term_statistics(&index, term)?;
                terms.insert(term.clone(), held);
            }
            let mut blends = Vec::with_capacity(starred.len());
            for alternatives in &starred {
                let blend = blended(transaction, &index, alternatives)?;
                for term in &blend.expansions {
                    if !terms.contains_key(term) {
                        let held = transaction.term_statistics(&index, term)?;
                        terms.insert(term.clone(), held);
                    }
                }
                blends.push(blend);
            }
            // ADR-0103: this node's index describes the shards it holds, and the
            // leaders of the others count theirs. A term's figure is then a
            // count alone — the bounds a pruned walk reads describe this node's
            // postings, and a gathered read walks none of them.
            let distinct: Vec<String> = terms.keys().cloned().collect();
            let elsewhere = self.counted_elsewhere(transaction, table, &index, &distinct)?;
            let statistics = match &elsewhere {
                Some(counted) => {
                    for (term, more) in distinct.iter().zip(&counted.holding) {
                        if let Some(held) = terms.get_mut(term) {
                            *held = TermStatistics::new(held.documents.saturating_add(*more));
                        }
                    }
                    SearchStatistics::new(
                        statistics.documents.saturating_add(counted.documents),
                        statistics.terms.saturating_add(counted.tokens),
                    )
                }
                None => statistics,
            };
            corpora.insert(
                path.clone(),
                Ranked {
                    corpus: Corpus {
                        documents: statistics.documents,
                        average_length: statistics.average_length().unwrap_or_default(),
                        terms,
                        asked,
                        blends,
                    },
                    index,
                    from_text: elsewhere.is_some(),
                },
            );
        }

        // Last, because it is the only thing here that reads the term dictionary
        // rather than the catalog, and because it needs the analyzers the loops
        // above resolved. Still before any access path exists, which is the
        // property that matters: the suggestion is a fact about the query and
        // the collection, so a read must not be able to earn a different one by
        // being planned differently.
        //
        // A hidden field consults no dictionary at all — not even to say
        // `NothingNearer`, which would tell the caller every word they typed is
        // in a field they cannot read.
        matched.retain(|(path, _)| !hidden(path));
        // ADR-0103 D3: on a node holding part of the table, its dictionary is
        // part of the collection's, and "nothing nearer" from it is false
        // whenever the nearer word sits in a shard it lacks.
        let suggestion = if self.missing(transaction, table, Part::Whole)?.is_some() {
            None
        } else {
            let indexes = Catalog::new(transaction).indexes_on(table)?;
            suggested(transaction, &indexes, &analyzers, &matched)?
        };

        Ok(Searched {
            analyzers,
            corpora,
            wanted: asked_of,
            suggestion,
        })
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
    phrased: &mut Vec<(&'a Path, &'a Expr)>,
) {
    match &expr.kind {
        ExprKind::Binary {
            op: BinaryOp::Matches,
            left,
            right,
        } => {
            if let ExprKind::Path(field) = &left.kind {
                into.push(&field.path);
                phrased.push((&field.path, right));
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
        // A highlight names a searched field and asks nothing of its own: the
        // query it marks against is whatever the rest of the statement asked of
        // that same path. So it is registered as searched — which is what
        // resolves the analyzer — and contributes no query.
        ExprKind::Call {
            function: Function::SearchHighlight,
            arguments,
            ..
        } => {
            if let Some(ExprKind::Path(field)) = arguments.first().map(|first| &first.kind) {
                into.push(&field.path);
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

/// `PrefixTooShort` for the first prefix typed shorter than the floor.
///
/// The **typed** spelling decides, which is the first of the alternatives — the
/// rule the `MATCHES PREFIX` loop above states.
fn too_short(prefixes: &[&[String]], span: Span) -> Result<()> {
    for alternatives in prefixes {
        if let Some(prefix) = alternatives.first()
            && prefix.chars().count() < SEARCH_PREFIX_MINIMUM
        {
            return Err(Error::PrefixTooShort {
                prefix: prefix.clone(),
                minimum: SEARCH_PREFIX_MINIMUM,
                span,
            });
        }
    }
    Ok(())
}

/// The terms one starred word blends, the most-held first (ADR-0104 D4).
///
/// Every term beginning with either spelling, up to the examination ceiling,
/// ranked by how many records hold it and cut at the expansion cap by that rank
/// — never in dictionary order, which would keep whichever rare words sort first
/// and drop the common one being typed. Ties fall to the term, so the same
/// dictionary always blends the same terms.
fn blended(
    transaction: &Transaction<'_>,
    index: &IndexDefinition,
    alternatives: &[String],
) -> Result<Blend> {
    let mut reached = std::collections::BTreeSet::new();
    for spelling in alternatives {
        let found =
            transaction.terms_with_prefix(index, spelling, SEARCH_PREFIX_SCORE_EXAMINATION_CAP)?;
        reached.extend(found.terms);
    }
    let mut ranked = Vec::with_capacity(reached.len());
    for term in reached {
        ranked.push((transaction.document_frequency(index, &term)?, term));
    }
    ranked.sort_by(|(left, one), (right, other)| right.cmp(left).then_with(|| one.cmp(other)));
    ranked.truncate(SEARCH_PREFIX_EXPANSION_CAP);
    Ok(Blend {
        documents: ranked.first().map_or(0, |(held, _)| *held),
        expansions: ranked.into_iter().map(|(_, term)| term).collect(),
    })
}
