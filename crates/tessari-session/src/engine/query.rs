//! What a `FROM SEARCH` query asks, and whether one field's words answer it.
//!
//! The query language is `MATCHES`'s own — quoted phrases with slop, `OR`,
//! `NOT`, starred words — read by the same [`crate::search::asked`], so a query
//! string means one thing whether it is asked of a field or of a search. What a
//! search adds is decided here and nowhere else: stop words leave the
//! conjunction, and a word is answered in a field by that field's synonyms and
//! only by the operators the field allows (ADR-0105).

use std::collections::{BTreeMap, BTreeSet};

use tessari_constants::SEARCH_PREFIX_MINIMUM;
use tessari_ql::{SearchOperator, Span};
use tessari_types::Analyzer;

use crate::error::{Error, Result};
use crate::search::{Asked, Word, asked, begins, malformed_slop, near, negation_without_term};

/// One word of a search query, as the dictionary is asked about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Probe {
    /// This term, whole — or one of the field's synonyms for it.
    Term(String),
    /// A term beginning with one of these spellings.
    Prefix(Vec<String>),
    /// A term within the edit budget of one of these spellings.
    Fuzzy(Vec<String>),
    /// A term containing this piece.
    Infix(String),
}

/// The query's shape: a phrase, or groups that must each be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    /// The words in order within `slop` extra tokens, inside one field.
    Phrase {
        /// The words.
        words: Vec<Probe>,
        /// The extra tokens the run may absorb.
        slop: usize,
    },
    /// Every group answered somewhere in the record, no excluded word held.
    Boolean {
        /// One group per `OR`-joined run.
        required: Vec<Vec<Probe>>,
        /// Words no answered record may hold.
        excluded: Vec<Probe>,
    },
}

/// A search query, read once per read.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    /// What a record must hold.
    pub(crate) shape: Shape,
    /// The words a score weighs, in the order written, stop words removed.
    pub(crate) scored: Vec<Probe>,
}

impl Query {
    /// Whether nothing is left to ask — every word a stop word, or none typed.
    pub(crate) fn is_empty(&self) -> bool {
        match &self.shape {
            Shape::Phrase { words, .. } => words.is_empty(),
            Shape::Boolean { required, .. } => required.is_empty(),
        }
    }

    /// Every probe the query holds, required, excluded and phrased alike.
    pub(crate) fn probes(&self) -> Vec<&Probe> {
        match &self.shape {
            Shape::Phrase { words, .. } => words.iter().collect(),
            Shape::Boolean { required, excluded } => {
                required.iter().flatten().chain(excluded).collect()
            }
        }
    }
}

/// Read the query a `FROM SEARCH` was given.
///
/// # Errors
///
/// The refusals `MATCHES` makes of the same text — a malformed slop marker, a
/// negation with nothing required, a prefix or infix under the floor.
pub(crate) fn read_query(
    analyzer: &Analyzer,
    operator: SearchOperator,
    text: &str,
    stopwords: &BTreeSet<String>,
    span: Span,
) -> Result<Query> {
    let shape = match operator {
        SearchOperator::Words => {
            if let Some(marker) = malformed_slop(text) {
                return Err(Error::MalformedSlop {
                    marker: marker.to_owned(),
                    span,
                });
            }
            if negation_without_term(text) {
                return Err(Error::NegationWithoutTerm { span });
            }
            match asked(analyzer, text) {
                // A quoted phrase keeps its stop words: they hold positions.
                Asked::Phrase { words, slop } => Shape::Phrase {
                    words: words.into_iter().map(probe_of).collect(),
                    slop,
                },
                Asked::Boolean { required, excluded } => {
                    let kept = |word: &Probe| !matches!(word, Probe::Term(term) if stopwords.contains(term));
                    Shape::Boolean {
                        required: required
                            .into_iter()
                            .map(|group| {
                                group
                                    .into_iter()
                                    .map(probe_of)
                                    .filter(|word| kept(word))
                                    .collect::<Vec<_>>()
                            })
                            .filter(|group| !group.is_empty())
                            .collect(),
                        excluded: excluded.into_iter().map(probe_of).filter(kept).collect(),
                    }
                }
            }
        }
        SearchOperator::Prefix | SearchOperator::Fuzzy | SearchOperator::Infix => {
            let typed = analyzer.prefixes(text);
            for alternatives in &typed {
                if let Some(first) = alternatives.first()
                    && first.chars().count() < SEARCH_PREFIX_MINIMUM
                    && operator != SearchOperator::Fuzzy
                {
                    return Err(Error::PrefixTooShort {
                        prefix: first.clone(),
                        minimum: SEARCH_PREFIX_MINIMUM,
                        span,
                    });
                }
            }
            Shape::Boolean {
                required: typed
                    .into_iter()
                    .filter_map(|alternatives| {
                        let probe = match operator {
                            SearchOperator::Prefix => Probe::Prefix(alternatives),
                            SearchOperator::Fuzzy => Probe::Fuzzy(alternatives),
                            // The spelling as typed, unstemmed: a piece of a
                            // word stems into nothing (ADR-0105 D9).
                            _ => Probe::Infix(alternatives.into_iter().next()?),
                        };
                        Some(vec![probe])
                    })
                    .collect(),
                excluded: Vec::new(),
            }
        }
    };
    let scored = match &shape {
        Shape::Phrase { words, .. } => words.clone(),
        Shape::Boolean { required, .. } => required.iter().flatten().cloned().collect(),
    };
    Ok(Query { shape, scored })
}

fn probe_of(word: Word) -> Probe {
    match word {
        Word::Term(term) => Probe::Term(term),
        Word::Prefix(alternatives) => Probe::Prefix(alternatives),
    }
}

/// How one field answers words: what it allows and its synonyms.
#[derive(Debug, Clone)]
pub(crate) struct Answering {
    /// Whether a fuzzy word may be answered here.
    pub(crate) fuzzy: bool,
    /// Whether a prefix or infix word may be answered here.
    pub(crate) prefix: bool,
    /// Whether a phrase may be answered here.
    pub(crate) phrase: bool,
    /// Each analysed word and the analysed alternatives that also answer it.
    pub(crate) synonyms: BTreeMap<String, Vec<String>>,
}

impl Answering {
    /// Whether the stored term `held` answers `probe` in this field.
    pub(crate) fn answers(&self, probe: &Probe, held: &str) -> bool {
        match probe {
            Probe::Term(term) => {
                term == held
                    || self
                        .synonyms
                        .get(term)
                        .is_some_and(|alternatives| alternatives.iter().any(|one| one == held))
            }
            Probe::Prefix(alternatives) => self.prefix && begins(alternatives, held),
            Probe::Fuzzy(alternatives) => self.fuzzy && near(alternatives, held),
            Probe::Infix(piece) => self.prefix && held.contains(piece.as_str()),
        }
    }

    /// The ordinals of a run of `words` in `held`, within `slop` — or none.
    ///
    /// The phrase walk of `MATCHES` (`search::matching::run_of`), asking this
    /// field's [`Self::answers`] at each step so a phrase word is answered by
    /// the field's synonyms and options too.
    pub(crate) fn run_of(
        &self,
        held: &[String],
        words: &[Probe],
        slop: usize,
    ) -> Option<Vec<usize>> {
        if !self.phrase {
            return None;
        }
        let first = words.first()?;
        let limit = words.len().saturating_sub(1).saturating_add(slop);
        held.iter().enumerate().find_map(|(start, token)| {
            if !self.answers(first, token) {
                return None;
            }
            let mut at = start;
            let mut walked = vec![start];
            for word in words.get(1..).unwrap_or_default() {
                let found = held
                    .iter()
                    .skip(at.saturating_add(1))
                    .position(|candidate| self.answers(word, candidate))?;
                at = at.saturating_add(1).saturating_add(found);
                walked.push(at);
            }
            (at.saturating_sub(start) <= limit).then_some(walked)
        })
    }
}

/// Whether a record — its fields' analysed terms, each beside how that field
/// answers — holds the query.
///
/// The conjunction is over the record as **one document**: a group is answered
/// when any field holds any of its words. A phrase must sit inside one field,
/// because a run of tokens across a field boundary is not a run in any text.
pub(crate) fn holds(query: &Query, fields: &[(&Answering, &[String])]) -> bool {
    let held = |probe: &Probe| {
        fields
            .iter()
            .any(|(answering, terms)| terms.iter().any(|term| answering.answers(probe, term)))
    };
    match &query.shape {
        Shape::Phrase { words, slop } => fields
            .iter()
            .any(|(answering, terms)| answering.run_of(terms, words, *slop).is_some()),
        Shape::Boolean { required, excluded } => {
            !required.is_empty()
                && required.iter().all(|group| group.iter().any(held))
                && !excluded.iter().any(held)
        }
    }
}
