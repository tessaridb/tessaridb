//! `FROM SEARCH`: resolving a search, nominating, re-testing and ranking.
//!
//! # The index nominates; the record decides
//!
//! Each member's postings say which records hold the query's words in some
//! field. That is a **candidate set**: every candidate is read, redacted for
//! the caller, analysed with the search's analyzer and asked the whole query on
//! the fields the caller may read — and that same analysis is what the score is
//! computed from. So a field the caller cannot read neither matches nor scores,
//! and nothing in the index has to know about grants (ADR-0105 D2).
//!
//! # What the read reaches
//!
//! The members of the current database, minus every table the caller may not
//! read and every member with a field the caller may not read: a member's
//! dictionary and statistics cannot say which field a word came from, so a
//! partial view of it would leak through the collection numbers.

mod ranking;

use std::collections::{BTreeMap, BTreeSet};

use tessari_constants::{
    SEARCH_FUZZY_EXPANSION_CAP, SEARCH_FUZZY_PREFIX, SEARCH_PREFIX_EXPANSION_CAP,
};
use tessari_ql::{Expr, Name, SearchOperator, Span};
use tessari_storage::{Catalog, IndexDefinition, RecordAddress, Transaction, WordSetKind};
use tessari_types::{Analyzer, Memo, Path, RecordId, Value};

use super::query::{Answering, Probe, Query, Shape, Text, holds, read_query};
use super::score::{Collection, Scored, bm25f, total};
use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::evaluate::{Part, Scope};
use crate::noticed::Noticed;
use crate::search::{Searched, budget};
use crate::session::Session;

/// One field of a member, ready to answer and to score.
#[derive(Debug)]
pub(crate) struct Field {
    /// Where the text is.
    pub(crate) path: Path,
    /// How it answers words.
    pub(crate) answering: Answering,
    /// Its BM25F weight.
    pub(crate) weight: f64,
    /// Whether a snippet may come from it.
    pub(crate) snippet: bool,
    /// Its average length across the member's documents.
    pub(crate) average: Option<f64>,
}

/// One table of the search, as this read reaches it.
#[derive(Debug)]
pub(crate) struct Member {
    /// The member index.
    pub(crate) index: IndexDefinition,
    /// The table's name, as the answer reports it.
    pub(crate) table: String,
    /// Its fields.
    pub(crate) fields: Vec<Field>,
    /// Documents it holds.
    pub(crate) documents: u64,
}

/// A search as one read reaches it.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// The analyzer the query and every field are read with.
    pub(crate) analyzer: Analyzer,
    /// The stop words, analysed.
    pub(crate) stopwords: BTreeSet<String>,
    /// The members this caller may read.
    pub(crate) members: Vec<Member>,
    /// One note per member an earlier tokenizer may have built (G058 C3).
    pub(crate) rebuild: Vec<crate::Note>,
}

/// One answered record.
#[derive(Debug)]
pub(crate) struct Found {
    /// Which member it came from.
    pub(crate) member: usize,
    /// Its identity.
    pub(crate) id: RecordId,
    /// The record, as the caller may see it.
    pub(crate) record: Value,
    /// Its score.
    pub(crate) score: f64,
}

/// What a read produced: the ranking, and whether any member was scanned.
pub(crate) struct Ranking {
    /// The records, best first.
    pub(crate) found: Vec<Found>,
    /// Whether every member was served by its postings.
    pub(crate) served: bool,
    /// Whether every member was also decided and scored from its postings,
    /// reading only the records it answered (Q-870).
    pub(crate) from_postings: bool,
}

impl Session<'_> {
    /// The search `name` in the current database, narrowed to what this
    /// session may read.
    pub(crate) fn resolve_search(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
    ) -> Result<Resolved> {
        let context = self.context(transaction, None, name.span)?;
        let members = Catalog::new(transaction).members_of(
            context.namespace,
            context.database,
            &name.text,
        )?;
        let Some(first) = members.first().and_then(|member| member.engine.clone()) else {
            return Err(Error::Unknown {
                entity: "search",
                name: name.text.clone(),
                span: name.span,
            });
        };
        let analyzer = Catalog::new(transaction)
            .analyzers()?
            .into_iter()
            .find(|held| held.name == first.analyzer)
            .ok_or_else(|| Error::Unknown {
                entity: "analyzer",
                name: first.analyzer.clone(),
                span: name.span,
            })?
            .analyzer;
        let stopwords = match &first.stopwords {
            Some(set) => self
                .word_set(transaction, WordSetKind::Stopwords, set, name.span)?
                .into_keys()
                .flat_map(|word| analyzer.terms(&word))
                .collect(),
            None => BTreeSet::new(),
        };
        let readable = self.readable_in(transaction)?;
        let mut reached = Vec::new();
        let mut rebuild = Vec::new();
        for index in members {
            let Some(engine) = index.engine.clone() else {
                continue;
            };
            if readable
                .as_ref()
                .is_some_and(|tables| !tables.contains(&index.table))
            {
                continue;
            }
            let visible = self.visible_in(transaction, index.table)?;
            if let Some(visible) = &visible
                && index
                    .fields
                    .iter()
                    .any(|path| !visible.contains(path.root()))
            {
                continue;
            }
            // A partial holder's statistics are not the collection's (ADR-0105 D8).
            self.refuse_reading_a_part(transaction, index.table, Part::Whole)?;
            let Some(table) = Catalog::new(transaction).table(index.table)? else {
                continue;
            };
            // No scan stands in for a member — it is the search — so it is read
            // and the answer says it can miss records until it is defined again.
            if index.needs_rebuild() {
                rebuild.push(crate::Note::NeedsRebuild {
                    index: engine.search.clone(),
                    table: table.name.clone(),
                    built: index.tokenizer,
                    member: true,
                });
            }
            let (statistics, lengths) = transaction.member_statistics(&index)?;
            let mut fields = Vec::with_capacity(index.fields.len());
            for (at, (path, declared)) in index.fields.iter().zip(&engine.fields).enumerate() {
                let synonyms = match &declared.synonyms {
                    Some(set) => self
                        .word_set(transaction, WordSetKind::Synonyms, set, name.span)?
                        .into_iter()
                        .filter_map(|(word, alternatives)| {
                            let [word] = analyzer.terms(&word).try_into().ok()?;
                            let alternatives: Vec<String> = alternatives
                                .iter()
                                .filter_map(|one| {
                                    let [one]: [String; 1] = analyzer.terms(one).try_into().ok()?;
                                    Some(one)
                                })
                                .collect();
                            Some((word, alternatives))
                        })
                        .collect(),
                    None => BTreeMap::new(),
                };
                let held = lengths.get(at).copied().unwrap_or(0);
                fields.push(Field {
                    path: path.clone(),
                    answering: Answering {
                        fuzzy: declared.fuzzy,
                        prefix: declared.prefix,
                        phrase: declared.phrase,
                        synonyms,
                    },
                    weight: f64::from(declared.weight) / f64::from(tessari_storage::UNIT_WEIGHT),
                    snippet: declared.snippet,
                    average: (statistics.documents > 0 && held > 0)
                        .then(|| total(held) / total(statistics.documents)),
                });
            }
            reached.push(Member {
                index,
                table: table.name,
                fields,
                documents: statistics.documents,
            });
        }
        reached.sort_by(|left, right| left.table.cmp(&right.table));
        Ok(Resolved {
            analyzer,
            stopwords,
            members: reached,
            rebuild,
        })
    }

    /// A declared word set's entries, refused by name when it has gone.
    fn word_set(
        &self,
        transaction: &mut Transaction<'_>,
        kind: WordSetKind,
        name: &str,
        span: Span,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        Catalog::new(transaction)
            .word_set(kind, name)?
            .map(|set| set.entries)
            .ok_or_else(|| Error::Unknown {
                entity: kind.word(),
                name: name.to_owned(),
                span,
            })
    }

    /// Whether every member's postings can nominate this query's records, and
    /// whether they can also decide and score it (Q-870) — the plan's
    /// questions, answered from the dictionary and one posting per member
    /// without a record.
    pub(crate) fn search_served(
        &self,
        transaction: &mut Transaction<'_>,
        resolved: &Resolved,
        (operator, text, span): (SearchOperator, &str, Span),
        unconditioned: bool,
    ) -> Result<(bool, bool)> {
        let query = read_query(
            &resolved.analyzer,
            operator,
            text,
            &resolved.stopwords,
            span,
        )?;
        let probes = query.probes();
        let mut decided = unconditioned
            && !query.is_empty()
            && super::postings::decidable(&query)
            && indexes_agree(transaction, &resolved.members)?;
        for member in &resolved.members {
            let mut expansions = Vec::with_capacity(probes.len());
            for probe in &probes {
                expansions.push(expand(transaction, member, probe)?);
            }
            if groups_of(&query, &probes, &expansions).is_none() {
                return Ok((false, false));
            }
            decided = decided
                && expansions.iter().all(Option::is_some)
                && !transaction.writes_in(
                    member.index.namespace,
                    member.index.database,
                    member.index.table,
                )
                && transaction.member_fielded(&member.index)?;
        }
        Ok((true, decided))
    }
}

/// The terms of `member`'s dictionary one probe reaches — `None` when the walk
/// would be past its cap, or the probe cannot be walked, and the member must be
/// scanned instead.
fn expand(
    transaction: &Transaction<'_>,
    member: &Member,
    probe: &Probe,
) -> Result<Option<BTreeSet<String>>> {
    let mut reached = BTreeSet::new();
    match probe {
        Probe::Term(term) => {
            reached.insert(term.clone());
            for field in &member.fields {
                if let Some(alternatives) = field.answering.synonyms.get(term) {
                    reached.extend(alternatives.iter().cloned());
                }
            }
        }
        Probe::Prefix(alternatives) => {
            for spelling in alternatives {
                let found = transaction.terms_with_prefix(
                    &member.index,
                    spelling,
                    SEARCH_PREFIX_EXPANSION_CAP,
                )?;
                if found.capped {
                    return Ok(None);
                }
                reached.extend(found.terms);
            }
        }
        Probe::Fuzzy(alternatives) => {
            for spelling in alternatives {
                // The terms near the spelling, and the terms of the surfaces
                // near it (Q-867).
                for found in [
                    transaction.terms_within_distance(
                        &member.index,
                        spelling,
                        budget(spelling),
                        SEARCH_FUZZY_PREFIX,
                        SEARCH_FUZZY_EXPANSION_CAP,
                    )?,
                    // A member built before surfaces existed holds none, and
                    // is scanned instead.
                    match transaction.terms_by_surface(
                        &member.index,
                        spelling,
                        budget(spelling),
                        SEARCH_FUZZY_PREFIX,
                        SEARCH_FUZZY_EXPANSION_CAP,
                    )? {
                        Some(found) => found,
                        None => return Ok(None),
                    },
                ] {
                    if found.capped {
                        return Ok(None);
                    }
                    reached.extend(found.terms);
                }
            }
        }
        Probe::Infix(piece) => {
            match transaction.terms_with_infix(&member.index, piece, SEARCH_PREFIX_EXPANSION_CAP)? {
                Some(found) if !found.capped => reached.extend(found.terms),
                _ => return Ok(None),
            }
        }
    }
    Ok(Some(reached))
}

/// The groups of terms a member's postings are asked for, or `None` when one
/// of the words could not be expanded and the member is scanned.
///
/// An excluded word nominates nothing: a negation names the complement of a
/// posting list. A phrase asks for each of its words.
fn groups_of(
    query: &Query,
    probes: &[&Probe],
    expansions: &[Option<BTreeSet<String>>],
) -> Option<Vec<Vec<String>>> {
    let terms_of = |probe: &Probe| -> Option<Vec<String>> {
        let at = probes.iter().position(|held| *held == probe)?;
        expansions
            .get(at)?
            .as_ref()
            .map(|found| found.iter().cloned().collect())
    };
    match &query.shape {
        Shape::Phrase { words, .. } => words.iter().map(terms_of).collect(),
        Shape::Boolean { required, .. } => required
            .iter()
            .map(|group| {
                let mut union = Vec::new();
                for probe in group {
                    union.extend(terms_of(probe)?);
                }
                Some(union)
            })
            .collect(),
    }
}

/// Whether every member's index may answer at this transaction's snapshot:
/// none behind the tail, and none in a table where a transaction across
/// leaders is part-way here (Q-919).
fn indexes_agree(transaction: &Transaction<'_>, members: &[Member]) -> Result<bool> {
    for member in members {
        if !transaction.indexes_are_current_for(member.index.table)? {
            return Ok(false);
        }
    }
    Ok(true)
}
