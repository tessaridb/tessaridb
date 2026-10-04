//! `DEFINE SEARCH`: several fields of several tables ranked as one collection
//! (ADR-0100 D2, ADR-0105).
//!
//! Three files, split by what each answers: [`query`] is what the query string
//! asks and whether one field's words answer it, [`score`] is BM25F, and
//! [`read`] is everything that touches the catalog and the records. This file
//! holds what an answered record carries into its projection — the
//! [`Hit`] — and the statements that declare and describe a search.

mod complete;
mod define;
mod postings;
mod query;
mod read;
mod score;
mod snippet;

use std::collections::BTreeMap;

use tessari_ql::{Expr, ExprKind, Function, SearchAsk, Select, Source, Span};
use tessari_storage::Transaction;
use tessari_types::{Analyzer, Number, RecordId, Value};

pub(crate) use define::{refuse_named_by_a_search, searches_script, word_sets_script};
pub(crate) use query::Query;
pub(crate) use read::{Member, Resolved};

use crate::error::{Error, Result};
use crate::evaluate::{Answered, Scope, alone, asserted};
use crate::noticed::Noticed;
use crate::outcome::{AccessPath, Note};
use crate::plan::Plan;
use crate::search::Searched;
use crate::session::Session;

/// What one answered record carries into its projection.
#[derive(Debug)]
pub(crate) struct Hit<'r> {
    /// The member it came from.
    pub(crate) member: &'r Member,
    /// The search's analyzer.
    pub(crate) analyzer: &'r Analyzer,
    /// The query that answered it.
    pub(crate) query: &'r Query,
    /// Its score.
    pub(crate) score: f64,
}

/// The text a `FROM SEARCH` asks, evaluated once.
fn asked_text(
    session: &Session<'_>,
    transaction: &mut Transaction<'_>,
    asked: &Expr,
) -> Result<String> {
    match session.evaluate(transaction, asked)? {
        Value::String(text) => Ok(text),
        other => Err(Error::SearchNeedsText {
            found: other.type_name(),
            span: asked.span,
        }),
    }
}

impl Session<'_> {
    /// The plan a `FROM SEARCH` reports: served by its members' postings, or
    /// scanned where a word could not be walked.
    fn search_plan(name: &str, served: bool, from_postings: bool) -> Plan {
        Plan {
            source: Some("search"),
            index: Some(name.to_owned()),
            // Q-870: ranked from the postings alone, or from each candidate's
            // re-analysed text.
            shape: Some(if from_postings {
                "search from postings"
            } else {
                "search"
            }),
            ..Plan::new(if served {
                AccessPath::Index
            } else {
                AccessPath::Scan
            })
        }
    }

    /// The plan `EXPLAIN` reports for a `FROM SEARCH`, read from the
    /// dictionary alone.
    pub(crate) fn explain_search(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Plan> {
        let Source::Search {
            name,
            ask,
            condition,
        } = &select.from
        else {
            return Ok(Plan::new(AccessPath::Scan));
        };
        let resolved = self.resolve_search(transaction, name)?;
        let (served, from_postings) = match ask {
            SearchAsk::Matches { operator, query } => {
                let text = asked_text(self, transaction, query)?;
                self.search_served(
                    transaction,
                    &resolved,
                    (*operator, &text, query.span),
                    condition.is_none(),
                )?
            }
            SearchAsk::Complete { .. } => (true, false),
        };
        Ok(Self::search_plan(&name.text, served, from_postings))
    }

    /// The records a `FROM SEARCH` answers, ranked, without their hits — the
    /// source a grouped read folds (ADR-0105 D6: facets are `GROUP BY` over
    /// the search).
    pub(crate) fn search_records(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        noticed: &Noticed,
    ) -> Result<(Vec<(RecordId, Value)>, Plan)> {
        let Source::Search {
            name,
            ask,
            condition,
        } = &select.from
        else {
            return Ok((Vec::new(), Plan::new(AccessPath::Scan)));
        };
        let resolved = self.resolve_search(transaction, name)?;
        let SearchAsk::Matches { operator, query } = ask else {
            let beginning = ask_beginning(ask);
            let text = asked_text(self, transaction, beginning)?;
            return Ok((
                self.complete(transaction, &resolved, &text, beginning.span)?,
                Self::search_plan(&name.text, true, false),
            ));
        };
        let text = asked_text(self, transaction, query)?;
        let (_, ranking) = self.rank_search(
            transaction,
            &resolved,
            (*operator, &text, query.span),
            condition.as_deref(),
            noticed,
        )?;
        let plan = Self::search_plan(&name.text, ranking.served, ranking.from_postings);
        Ok((
            ranking
                .found
                .into_iter()
                .map(|found| (found.id, found.record))
                .collect(),
            plan,
        ))
    }

    /// The whole answer of an ungrouped `FROM SEARCH`: ranked, bounded, then
    /// projected with each record's hit in scope.
    pub(crate) fn search_answer(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        noticed: &Noticed,
        mut notes: Vec<Note>,
    ) -> Result<Answered> {
        let Source::Search {
            name,
            ask,
            condition,
        } = &select.from
        else {
            return Err(Error::Unknown {
                entity: "search",
                name: String::new(),
                span: select.span,
            });
        };
        if let Some(ordering) = select.order.first() {
            return Err(Error::SearchIsItsOwnOrder {
                span: ordering.key.span,
            });
        }
        let resolved = self.resolve_search(transaction, name)?;
        notes.extend(resolved.rebuild.iter().cloned());
        let skip = select
            .start
            .map_or(0, |start| usize::try_from(start).unwrap_or(usize::MAX));
        let keep = select.limit.map_or(usize::MAX, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        });
        let wanted = self.shaped(transaction, select)?;
        let searched = Searched::default();
        let SearchAsk::Matches { operator, query } = ask else {
            let beginning = ask_beginning(ask);
            let text = asked_text(self, transaction, beginning)?;
            let completed = self.complete(transaction, &resolved, &text, beginning.span)?;
            let plan = Self::search_plan(&name.text, true, false);
            let mut answered = Vec::new();
            for (id, record) in completed.into_iter().skip(skip).take(keep) {
                let shaped = match &wanted {
                    Some(wanted) => {
                        self.project(transaction, &id, &record, wanted, &searched, noticed)?
                    }
                    None => record,
                };
                answered.push((id, shaped));
            }
            asserted(select, &plan)?;
            notes.extend(noticed.drained());
            alone(select, &answered)?;
            return Ok(Answered {
                records: answered,
                plan,
                notes,
                suggestion: None,
            });
        };
        let text = asked_text(self, transaction, query)?;
        let (asked, ranking) = self.rank_search(
            transaction,
            &resolved,
            (*operator, &text, query.span),
            condition.as_deref(),
            noticed,
        )?;
        let plan = Self::search_plan(&name.text, ranking.served, ranking.from_postings);
        let mut answered = Vec::new();
        for found in ranking.found.into_iter().skip(skip).take(keep) {
            let Some(member) = resolved.members.get(found.member) else {
                continue;
            };
            let hit = Hit {
                member,
                analyzer: &resolved.analyzer,
                query: &asked,
                score: found.score,
            };
            let shaped = match &wanted {
                Some(wanted) => self.project_with(
                    transaction,
                    &found.id,
                    &found.record,
                    wanted,
                    (&searched, noticed),
                    (None, Some(&hit)),
                )?,
                None => found.record,
            };
            answered.push((found.id, shaped));
        }
        asserted(select, &plan)?;
        notes.extend(noticed.drained());
        alone(select, &answered)?;
        Ok(Answered {
            records: answered,
            plan,
            notes,
            suggestion: None,
        })
    }

    /// `search::score()`, `search::table_name()`, `search::snippet()` and a
    /// `search::highlight(field)` asked inside a `FROM SEARCH` — `None` for any
    /// other call, which the evaluator then answers as it always has.
    pub(crate) fn answered_by_hit(
        &self,
        transaction: &mut Transaction<'_>,
        function: Function,
        arguments: &[Expr],
        scope: Scope<'_>,
        span: Span,
    ) -> Result<Option<Value>> {
        let hit = scope.hit;
        let asks = match function {
            Function::SearchScore => arguments.is_empty(),
            Function::SearchTable | Function::SearchSnippet => true,
            Function::SearchHighlight => hit.is_some(),
            _ => false,
        };
        if !asks {
            return Ok(None);
        }
        let Some(hit) = hit else {
            return Err(Error::NotSearched { span });
        };
        Ok(Some(match function {
            Function::SearchScore => Value::Number(Number::float(hit.score)),
            Function::SearchTable => Value::from(hit.member.table.as_str()),
            Function::SearchSnippet => snippet::best(hit, scope.record),
            _ => {
                let none = Value::Array(Vec::new());
                let Some(first) = arguments.first() else {
                    return Ok(Some(none));
                };
                let ExprKind::Path(field) = &first.kind else {
                    return Ok(Some(none));
                };
                let Value::String(text) = self.evaluate_in(transaction, first, scope)? else {
                    return Ok(Some(none));
                };
                snippet::marks(hit, &field.path, &text)
            }
        }))
    }
}

fn ask_beginning(ask: &SearchAsk) -> &Expr {
    match ask {
        SearchAsk::Complete { beginning } => beginning,
        SearchAsk::Matches { query, .. } => query,
    }
}

/// `{ start, end }` as the highlight answers it.
fn range(start: usize, end: usize) -> Value {
    let at = |byte: usize| Value::Number(Number::Integer(i64::try_from(byte).unwrap_or(i64::MAX)));
    Value::Object(BTreeMap::from([
        ("start".to_owned(), at(start)),
        ("end".to_owned(), at(end)),
    ]))
}
