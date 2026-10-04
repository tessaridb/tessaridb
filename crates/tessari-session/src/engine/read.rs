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

    /// Every record of the search that answers the query, ranked.
    pub(crate) fn rank_search(
        &self,
        transaction: &mut Transaction<'_>,
        resolved: &Resolved,
        (operator, text, span): (SearchOperator, &str, Span),
        condition: Option<&Expr>,
        noticed: &Noticed,
    ) -> Result<(Query, Ranking)> {
        let query = read_query(
            &resolved.analyzer,
            operator,
            text,
            &resolved.stopwords,
            span,
        )?;
        if query.is_empty() {
            return Ok((
                query,
                Ranking {
                    found: Vec::new(),
                    served: true,
                    from_postings: false,
                },
            ));
        }
        let mut served = true;
        let mut expansions: Vec<Vec<Option<BTreeSet<String>>>> = Vec::new();
        for member in &resolved.members {
            let mut per_probe = Vec::new();
            for probe in query.probes() {
                per_probe.push(expand(transaction, member, probe)?);
            }
            expansions.push(per_probe);
        }
        // One document frequency per scored word, blended over the terms that
        // answer it and summed over the members the read reaches.
        let probes = query.probes();
        let mut holding = Vec::with_capacity(query.scored.len());
        for probe in &query.scored {
            let at = probes.iter().position(|held| *held == probe).unwrap_or(0);
            let mut terms: BTreeSet<&String> = BTreeSet::new();
            for member in &expansions {
                if let Some(Some(found)) = member.get(at) {
                    terms.extend(found);
                }
            }
            let mut largest = 0_u64;
            for term in terms {
                let mut documents = 0_u64;
                for member in &resolved.members {
                    documents = documents
                        .saturating_add(transaction.document_frequency(&member.index, term)?);
                }
                largest = largest.max(documents);
            }
            holding.push((probe, total(largest)));
        }
        let collection = Collection {
            documents: total(
                resolved
                    .members
                    .iter()
                    .fold(0_u64, |sum, member| sum.saturating_add(member.documents)),
            ),
        };

        // The candidates of one read share their words, so each is analysed
        // once per read (Q-870).
        let mut memo = Memo::default();
        // Q-870: whether the postings can decide this read, member by member.
        let decidable = condition.is_none()
            && super::postings::decidable(&query)
            && indexes_agree(transaction, &resolved.members)?;
        let mut from_postings = decidable;
        let mut found = Vec::new();
        for (at, member) in resolved.members.iter().enumerate() {
            if decidable
                && !transaction.writes_in(
                    member.index.namespace,
                    member.index.database,
                    member.index.table,
                )
                && let Some(reached) = expansions[at].iter().cloned().collect::<Option<Vec<_>>>()
            {
                let terms: Vec<String> = reached
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                if let Some(held) = transaction.member_postings(&member.index, &terms)? {
                    let visible = self.visible_in(transaction, member.index.table)?;
                    for (id, score) in super::postings::ranked(
                        member,
                        &query,
                        &reached,
                        &held,
                        (collection, &holding),
                    ) {
                        let address = RecordAddress::new(
                            member.index.namespace,
                            member.index.database,
                            member.index.table,
                            id.clone(),
                        );
                        let Some(payload) = transaction.get(&address)? else {
                            continue;
                        };
                        found.push(Found {
                            member: at,
                            id,
                            record: self.record_of(&payload, &visible)?,
                            score,
                        });
                    }
                    continue;
                }
            }
            from_postings = false;
            let nominated = match groups_of(&query, &probes, &expansions[at]) {
                Some(groups) => transaction
                    .member_candidates(&member.index, &groups)?
                    .into_iter()
                    .collect::<Vec<_>>(),
                None => {
                    served = false;
                    transaction
                        .scan_table(
                            member.index.namespace,
                            member.index.database,
                            member.index.table,
                        )?
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect()
                }
            };
            let visible = self.visible_in(transaction, member.index.table)?;
            for id in nominated {
                let address = RecordAddress::new(
                    member.index.namespace,
                    member.index.database,
                    member.index.table,
                    id.clone(),
                );
                let Some(payload) = transaction.get(&address)? else {
                    continue;
                };
                let record = self.record_of(&payload, &visible)?;
                let analysed: Vec<(Vec<String>, Vec<String>)> = member
                    .fields
                    .iter()
                    .map(|field| match field.path.resolve(&record) {
                        Some(Value::String(text)) => resolved.analyzer.analysed(text, &mut memo),
                        _ => (Vec::new(), Vec::new()),
                    })
                    .collect();
                let texts: Vec<Text<'_>> = analysed
                    .iter()
                    .map(|(terms, surfaces)| Text { terms, surfaces })
                    .collect();
                let fields: Vec<(&Answering, Text<'_>)> = member
                    .fields
                    .iter()
                    .zip(&texts)
                    .map(|(field, text)| (&field.answering, *text))
                    .collect();
                if !holds(&query, &fields) {
                    continue;
                }
                if let Some(condition) = condition {
                    let held = self.evaluate_in(
                        transaction,
                        condition,
                        Scope::searching(&record, &Searched::default())
                            .identified(&id)
                            .noticing(noticed),
                    )?;
                    if !boolean(&held, condition.span)? {
                        continue;
                    }
                }
                let scored: Vec<Scored<'_>> = member
                    .fields
                    .iter()
                    .zip(&texts)
                    .map(|(field, text)| Scored {
                        answering: &field.answering,
                        text: *text,
                        weight: field.weight,
                        average: field.average,
                    })
                    .collect();
                let score = bm25f(collection, &holding, &scored);
                found.push(Found {
                    member: at,
                    id,
                    record,
                    score,
                });
            }
        }
        found.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| {
                    resolved.members[left.member]
                        .table
                        .cmp(&resolved.members[right.member].table)
                })
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok((
            query,
            Ranking {
                found,
                served,
                from_postings,
            },
        ))
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
