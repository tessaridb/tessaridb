//! What a read answers with: projection, highlighting and ranking.

use std::collections::BTreeMap;

use tessari_encoding::Posting;
use tessari_ql::{Expr, ExprKind, Span};
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

use crate::error::{Error, Result};
use crate::noticed::Noticed;
use crate::rank::{Held, explain, score};
use crate::search::{Searched, marked, whole_terms};
use crate::session::Session;

use super::{Scope, Shaped, at, omit_within, omits};

impl Session<'_> {
    /// One record, reduced to the values a read asked for.
    ///
    /// **A projection that reaches nothing omits its field** rather than
    /// answering `none`. `Value::None` means the field is not there, so writing
    /// it into an object would say the field is there and holds
    /// not-being-there — the contradiction the value system spends its own rules
    /// avoiding. The consequence is that projected records keep differing
    /// shapes, which is the same property that makes a table able to hold
    /// documents at all.
    ///
    /// A computed projection is evaluated against this record, so it is the
    /// same evaluator a `WHERE` uses and cannot disagree with it.
    pub(crate) fn project(
        &self,
        transaction: &mut Transaction<'_>,
        id: &RecordId,
        record: &Value,
        wanted: &Shaped,
        searched: &Searched,
        noticed: &Noticed,
    ) -> Result<Value> {
        self.project_with(transaction, id, record, wanted, (searched, noticed), None)
    }

    /// [`Self::project`], with the record's ranks when a fused read projects it.
    pub(crate) fn project_with(
        &self,
        transaction: &mut Transaction<'_>,
        id: &RecordId,
        record: &Value,
        wanted: &Shaped,
        (searched, noticed): (&Searched, &Noticed),
        ranks: Option<&[Option<u64>]>,
    ) -> Result<Value> {
        // The star first, so a value written out by name is written **over** the
        // field it shares a name with. `SELECT *, upper(name) AS name` answers
        // with the computed one, which is the same rule the ordering stage's
        // overlay already follows — an alias shadows the field it is named for.
        let mut projected = match (wanted.everything, record) {
            (true, Value::Object(fields)) => fields
                .iter()
                .filter(|(name, _)| !omits(&wanted.omit, name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            // A star over something that is not an object contributes nothing
            // rather than failing: a projection reaching nothing omits its field
            // everywhere else here, and this is the same rule one level up.
            _ => BTreeMap::new(),
        };
        // A route that reaches inside is removed after the copy rather than
        // filtered during it, because the field it names still belongs in the
        // answer — `OMIT address.postcode` keeps the address.
        for route in &wanted.omit {
            if !route.path.steps().is_empty() {
                omit_within(&mut projected, &route.path);
            }
        }
        for value in &wanted.values {
            // A route reaching several values is **collected** here rather than
            // resolved, and the rule lives in the projection the way the
            // existential rule lives in the comparison. Reading it in the
            // evaluator's path arm instead would be fewer lines and would hand
            // `array::len(tags[*])` an answer nobody decided on — a rule kept
            // honest only by a parser refusal is a rule waiting for the day
            // somebody moves the refusal.
            //
            // A relation is **total**, so this always writes its field: every
            // record has a reach, and zero of them is an empty array rather than
            // an absence. An empty array, an absent field and a single value all
            // reach nothing and so all answer `[]` — a projection that told them
            // apart would be reading whether the field exists, which is a
            // different question that `tags` already answers on its own.
            if let ExprKind::Path(field) = &value.value.kind
                && field.path.is_several()
            {
                let reached = field.path.reach(record).into_iter().cloned().collect();
                projected.insert(value.name.text.clone(), Value::Array(reached));
                continue;
            }
            // The searched context reaches here as well as the `WHERE` and the
            // `ORDER BY`: a projection is where a caller most often asks for a
            // score, and it needs the same collection the ordering measures
            // against or the two would disagree in the same statement.
            //
            // A fold never reaches here: a projection holding one goes through
            // `grouped`, which is the only place many records become one.
            let scope = Scope::searching(record, searched)
                .identified(id)
                .noticing(noticed);
            let held = self.evaluate_in(
                transaction,
                &value.value,
                ranks.map_or(scope, |ranks| scope.with_ranks(ranks)),
            )?;
            if held.is_present() {
                projected.insert(value.name.text.clone(), held);
            }
        }
        Ok(Value::Object(projected))
    }

    /// What one record scores against the collection its field is indexed in.
    ///
    /// The first argument must be a **path**: a score is measured against the
    /// statistics of one indexed field, and an arbitrary expression names no
    /// field to have statistics for. Refusing that is refusing to guess.
    /// Where in this record's text the read's own query matched, as an ordered
    /// array of `{ start, end }` byte ranges.
    ///
    /// # Everything here answers `[]` rather than refusing
    ///
    /// A highlight is a projection, not a filter: it decorates records some
    /// other clause already chose. So a field with no declared analyzer, a
    /// record holding no text there, and a field nobody asked about all answer
    /// *no marks* — which is the true answer in each case, and is what lets one
    /// `search::highlight(body)` be written over a table whose records do not
    /// all carry a body.
    ///
    /// That is the opposite of [`rank`](Self::rank), which refuses, and the
    /// difference is real rather than a style choice: a score with nothing to
    /// measure against has no honest number, while a highlight with nothing to
    /// mark has an honest answer and it is the empty one.
    pub(super) fn highlight(
        &self,
        transaction: &mut Transaction<'_>,
        arguments: &[Expr],
        scope: Scope<'_>,
    ) -> Result<Value> {
        let none = Ok(Value::Array(Vec::new()));
        let Some(first) = arguments.first() else {
            return none;
        };
        // The argument is the field, and the field is where both the analyzer
        // and the recorded query are found — so an expression that is not a path
        // names neither and marks nothing.
        let ExprKind::Path(field) = &first.kind else {
            return none;
        };
        let Some(analyzer) = scope.analyzer(&field.path) else {
            return none;
        };
        // Evaluated first, whatever marks it: a field this session may not read
        // resolves to nothing here, and nothing is marked.
        let Value::String(text) = self.evaluate_in(transaction, first, scope)? else {
            return none;
        };
        let wanted = scope.wanted(&field.path);
        // An index keeping `OFFSETS` holds the bytes of every occurrence of a
        // term, so a whole-word query is marked from them without analysing the
        // text (ADR-0100 D4); anything else, or a posting without them, is
        // marked the way it always was.
        let stored = match (
            scope.offsets(&field.path),
            scope.id,
            whole_terms(analyzer, wanted),
        ) {
            (Some(index), Some(id), Some(terms)) => stored_marks(transaction, index, &terms, id)?,
            _ => None,
        };
        let marks = match stored {
            Some(marks) => marks,
            None => marked(analyzer, &text, wanted),
        };
        Ok(Value::Array(
            marks
                .into_iter()
                .map(|bytes| {
                    Value::Object(BTreeMap::from([
                        ("start".to_owned(), at(bytes.start)),
                        ("end".to_owned(), at(bytes.end)),
                    ]))
                })
                .collect(),
        ))
    }

    pub(super) fn rank(
        &self,
        transaction: &mut Transaction<'_>,
        arguments: &[Expr],
        scope: Scope<'_>,
        span: Span,
        explaining: bool,
    ) -> Result<Value> {
        let answer = |corpus: &crate::rank::Corpus, held: &Held| {
            if explaining {
                explain(corpus, held)
            } else {
                score(corpus, held)
            }
        };
        // The query is the second argument and it is deliberately **not**
        // evaluated here. It was evaluated and analysed once, while the corpus
        // was resolved, and doing it again per scored record is half of the cost
        // this function used to carry. Its presence is still what makes the call
        // a score rather than a mistake.
        let (Some(first), Some(_query)) = (arguments.first(), arguments.get(1)) else {
            return Ok(Value::None);
        };
        let ExprKind::Path(field) = &first.kind else {
            return Err(Error::NoSearchIndex {
                field: "that expression".to_owned(),
                span,
            });
        };
        // Refused before either argument is evaluated: there is nothing to
        // measure against, so evaluating them would be work done to reach a
        // conclusion already known.
        //
        // The record's identity is part of that. A row with none is a row no
        // index holds — a join's pair, a fold's result — so there are no postings
        // to read and no honest number to return, which is the same refusal for
        // the same reason.
        let (Some(ranked), Some(analyzer), Some(id)) = (
            scope.ranked(&field.path),
            scope.analyzer(&field.path),
            scope.id,
        ) else {
            return Err(Error::NoSearchIndex {
                field: field.path.to_string(),
                span,
            });
        };
        let corpus = &ranked.corpus;

        // The record's two numbers, read from the postings the writer already
        // put them in. `None` is the term not posted against this record, which
        // scores nothing — reached without touching the record at all.
        //
        // Every posting of one record carries the same length, written by one
        // analysis in one batch, so the last one read is as good as any. A record
        // holding none of the asked terms leaves it at zero, which changes
        // nothing: with no occurrences there is no term for the length to divide.
        let mut occurrences = BTreeMap::new();
        let mut length = 0_u32;
        // A gathered read's records are not in this node's postings, so every
        // one of them is scored from its text (ADR-0103 D2).
        let mut membership = ranked.from_text;
        for term in corpus.terms.keys().filter(|_| !ranked.from_text) {
            match transaction.posting(&ranked.index, term, id)? {
                None => {}
                Some(Posting::Counted {
                    frequency,
                    length: tokens,
                }) => {
                    occurrences.insert(term.clone(), frequency);
                    length = tokens;
                }
                // An index written before postings carried a payload. It knows
                // the term is here and not how often, so the numbers come from
                // the text — the old cost, paid only by an old index, exactly as
                // `document_frequency` falls through to its count.
                Some(Posting::Membership) => {
                    membership = true;
                    break;
                }
            }
        }
        if !membership {
            return Ok(answer(corpus, &Held::counted(occurrences, length)));
        }
        let held = self.evaluate_in(transaction, first, scope)?;
        let Value::String(text) = held else {
            // Not text: it holds none of the words, which scores zero. The same
            // answer a document of the wrong shape gets from `MATCHES`, in the
            // ranking's own terms.
            return Ok(answer(corpus, &Held::default()));
        };
        Ok(answer(
            corpus,
            &Held::analysed(analyzer, &text, &corpus.counted()),
        ))
    }
}

/// The byte ranges of every occurrence of these terms in one record, from an
/// index keeping `OFFSETS` — in text order, as `marked` returns them.
///
/// `None` when a posting of this record carries no offsets, so the caller
/// analyses the text instead of marking from a partial list.
fn stored_marks(
    transaction: &Transaction<'_>,
    index: &tessari_storage::IndexDefinition,
    terms: &std::collections::BTreeSet<String>,
    id: &tessari_types::RecordId,
) -> Result<Option<Vec<core::ops::Range<usize>>>> {
    let mut marks = Vec::new();
    for term in terms {
        let Some(located) = transaction.located(index, term, id)? else {
            continue;
        };
        if located.offsets.is_empty() {
            return Ok(None);
        }
        for (start, end) in located.offsets {
            let widen = |byte: u32| usize::try_from(byte).unwrap_or(usize::MAX);
            marks.push(widen(start)..widen(end));
        }
    }
    marks.sort_by_key(|bytes| bytes.start);
    marks.dedup();
    Ok(Some(marks))
}
