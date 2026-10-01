//! A read of a split table this node holds only part of (G033, ADR-0083).
//!
//! # The one question, and who answers it
//!
//! The session knows which shards a read needs and which of them this node was
//! served; it does not know where the others are, or how to reach them. That is
//! `tessari-wire`'s, which depends on this crate, so — exactly as for
//! [`crate::Elsewhere`] — the session declares the question and the crate that
//! greets peers answers it: *the stored records of this shard inside this
//! window, from that shard's leader.*
//!
//! # Records travel, the statement does not
//!
//! What comes back is stored records, and the statement runs here over this
//! node's own records and the gathered ones together. So a field this session
//! may not read is hidden from a gathered record by the same code that hides it
//! from a local one — a condition evaluated on the answering node would run
//! before this session's redaction, and a hidden field would become searchable.
//!
//! # Refused rather than answered in part
//!
//! A shard nobody answers for refuses the whole read, and so does a read whose
//! records pass [`GATHER_RECORDS`]. A partial answer is the silent wrong number
//! this store refuses everywhere else.

use tessari_constants::{GATHER_PAGE_RECORDS, GATHER_RECORDS};
use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{Catalog, ShardMap, ShardSpan, Transaction, Window};
use tessari_types::{DatabaseId, NamespaceId, RecordId, ShardId, TableId};

use crate::aggregate::{Groups, merge_partials, occurrences};
use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::evaluate::{Part, Scope};
use crate::noticed::Noticed;
use crate::outcome::Note;
use crate::session::Session;

/// Stored records, each with its identity, in identity order.
pub(crate) type Stored = Vec<(RecordId, Vec<u8>)>;

/// One question: the records of one shard inside one window.
#[derive(Debug, Clone, Copy)]
pub struct Asked<'a> {
    /// The table's namespace.
    pub namespace: NamespaceId,
    /// The table's database.
    pub database: DatabaseId,
    /// The table.
    pub table: TableId,
    /// The shard whose records are wanted.
    pub shard: ShardId,
    /// Which of its records: the shard's span narrowed to what the read needs.
    pub window: Window<'a>,
    /// The most records the answer may hold. An answer that would hold more is
    /// [`Unanswered::Ceiling`] and never a shortened one.
    pub most: usize,
    /// A condition the leader may narrow the records by first (ADR-0097); the
    /// asker tests every record again whatever the leader did with it.
    pub pushed: Option<&'a crate::Pushed>,
    /// The most records this shard need send, in identity order, when the read
    /// wants only its first few and the leader keeps exactly what this node
    /// would (ADR-0097 D2); `None` for all of them.
    pub enough: Option<usize>,
    /// The folds the leader may answer instead of the records (ADR-0097 D2);
    /// `None` to be sent the records.
    pub reduce: Option<&'a crate::Reduce>,
}

/// What a shard's leader answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Gathered {
    /// The stored records, in identity order.
    pub records: Vec<(RecordId, Vec<u8>)>,
    /// The node that answered.
    pub node: [u8; NODE_ID_LEN],
    /// What the leader folded the records into, when it was asked to; `None`
    /// from a leader that was not asked, and read as declined from one that was.
    pub reduced: Option<crate::Reduced>,
}

/// Why a shard's records did not arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unanswered {
    /// There were more of them than [`Asked::most`].
    Ceiling,
    /// Nobody could answer, in the words of whatever refused.
    Refused(String),
    /// The leader's map of the table no longer has the shard asked for, or does
    /// not have it yet (ADR-0095 D4).
    Moved,
}

/// Who fetches a shard's records for this node.
///
/// `Send + Sync` because one is shared by every connection a node serves.
pub trait Gather: core::fmt::Debug + Send + Sync {
    /// The records `asked` names, from the leader of `asked.shard`.
    ///
    /// # Errors
    ///
    /// [`Unanswered::Ceiling`] when there are more than `asked.most`, and
    /// [`Unanswered::Refused`] when no leader answered.
    fn gather(&self, asked: &Asked<'_>) -> core::result::Result<Gathered, Unanswered>;
}

/// The part of a read this node lacks, when it lacks any.
pub(crate) struct Missing {
    table: String,
    namespace: NamespaceId,
    database: DatabaseId,
    /// `None` for a table that is not split, which this node holds none of.
    map: Option<ShardMap>,
    /// Every shard the read needs, in key order.
    needed: Vec<ShardId>,
    /// Those of them this node was not served.
    lacking: Vec<ShardId>,
}

impl Missing {
    /// The refusal a shard that did not answer gives.
    fn unanswered(&self, shard: ShardId, why: Unanswered) -> Error {
        match why {
            Unanswered::Ceiling => self.too_much(),
            Unanswered::Moved => Error::ShardMapMoved {
                table: self.table.clone(),
                shard: shard.get(),
            },
            Unanswered::Refused(why) => Error::NotGathered {
                table: self.table.clone(),
                shard: shard.get(),
                why,
            },
        }
    }

    /// The refusal a read holding more than a gather may gives.
    fn too_much(&self) -> Error {
        Error::GatheredTooMuch {
            table: self.table.clone(),
            most: GATHER_RECORDS,
        }
    }

    /// The note a gathered answer carries.
    fn note(&self) -> Note {
        Note::Gathered {
            table: self.table.clone(),
            shards: self.lacking.iter().map(|shard| shard.get()).collect(),
        }
    }

    /// The refusal a node with no way to gather gives.
    pub(crate) fn refusal(&self) -> Error {
        Error::NotHeldHere {
            table: self.table.clone(),
            shards: self.lacking.iter().map(|shard| shard.get()).collect(),
        }
    }
}

impl Session<'_> {
    /// What of `part` of table `id` this node was not served, or `None` when it
    /// holds all of it (G031 S3.3).
    ///
    /// A node never served anything — a leader, a store standing alone — answers
    /// `None` for [`tessari_storage::Store::served`] and pays one in-memory read;
    /// so does every follower whose reach covers the table's database.
    pub(crate) fn missing(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        part: Part<'_>,
    ) -> Result<Option<Missing>> {
        let Some(over) = self.store.served() else {
            return Ok(None);
        };
        let Some(definition) = Catalog::new(transaction).table(id)? else {
            return Ok(None);
        };
        if over.contains(tessari_storage::Reach::Database(
            definition.namespace,
            definition.database,
        )) {
            return Ok(None);
        }
        let Some(map) = definition.shards else {
            return Ok(Some(Missing {
                table: definition.name,
                namespace: definition.namespace,
                database: definition.database,
                map: None,
                needed: Vec::new(),
                lacking: Vec::new(),
            }));
        };
        let holds = |shard: ShardId| {
            over.contains(tessari_storage::Reach::Shard(
                definition.namespace,
                definition.database,
                id,
                shard,
            ))
        };
        let needed: Vec<ShardId> = map
            .spans()
            .filter(|span| window_of(span, part).is_some())
            .map(|span| span.id)
            .collect();
        let lacking: Vec<ShardId> = needed
            .iter()
            .copied()
            .filter(|shard| !holds(*shard))
            .collect();
        if lacking.is_empty() {
            return Ok(None);
        }
        Ok(Some(Missing {
            table: definition.name,
            namespace: definition.namespace,
            database: definition.database,
            map: Some(map),
            needed,
            lacking,
        }))
    }

    /// Every record `part` of table `id` needs, this node's own and the ones
    /// gathered from the shards it lacks, in identity order — or `None` when
    /// this node holds all of them and the ordinary read answers.
    ///
    /// Refuses `NotHeldHere` exactly as [`Session::refuse_reading_a_part`] does
    /// wherever gathering is not possible: a node told of no gatherer, a table
    /// that is not split, and — because the gatherer is withheld there — inside
    /// a transaction and under `VERSION`, where one snapshot is the promise.
    pub(crate) fn gather_a_part(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        part: Part<'_>,
        pushed: Option<&crate::Pushed>,
        enough: Option<usize>,
    ) -> Result<Option<(Stored, Note)>> {
        let Some(missing) = self.missing(transaction, id, part)? else {
            return Ok(None);
        };
        let (Some(gatherer), Some(map)) = (self.gather.as_ref(), missing.map.as_ref()) else {
            return Err(missing.refusal());
        };
        let too_much = || missing.too_much();
        let mut found: Stored = Vec::new();
        for span in map.spans().filter(|span| missing.needed.contains(&span.id)) {
            let Some(window) = window_of(&span, part) else {
                continue;
            };
            // ADR-0097 D2: the spans are walked in key order, so a read that
            // needs its first `n` records has them once `n` are in hand, and a
            // shard past that point is not asked at all.
            let remaining = enough.map(|enough| enough.saturating_sub(found.len()));
            if remaining == Some(0) {
                break;
            }
            let most = GATHER_RECORDS.saturating_sub(found.len());
            let records = if missing.lacking.contains(&span.id) {
                let asked = Asked {
                    namespace: missing.namespace,
                    database: missing.database,
                    table: id,
                    shard: span.id,
                    window,
                    most,
                    pushed,
                    enough: remaining,
                    reduce: None,
                };
                match gatherer.gather(&asked) {
                    Ok(gathered) => gathered.records,
                    Err(why) => return Err(missing.unanswered(span.id, why)),
                }
            } else {
                // This node's own span is narrowed by the same condition before
                // it counts towards `enough`: a record the condition will drop
                // must not take the place of one it keeps — which is also why
                // the read is not cut at `enough` until it has been narrowed.
                let held = transaction.records_between(
                    missing.namespace,
                    missing.database,
                    id,
                    window,
                    None,
                    match (pushed, remaining) {
                        (None, Some(remaining)) => remaining.min(most.saturating_add(1)),
                        _ => most.saturating_add(1),
                    },
                )?;
                match pushed {
                    Some(pushed) => crate::keeping(self.store, pushed, held)?,
                    None => held,
                }
            };
            // Checked here as well as asked of the gatherer: the ceiling is this
            // node's memory, and an answerer that ignored it would otherwise be
            // trusted with it.
            if records.len() > most {
                return Err(too_much());
            }
            let mut records = records;
            if let Some(remaining) = remaining {
                records.truncate(remaining);
            }
            found.extend(records);
        }
        Ok(Some((found, missing.note())))
    }

    /// The groups a grouping read of the whole of table `id` folds into, this
    /// node's records offered and the leaders' partials merged, in key order
    /// (ADR-0097 D2) — or `None` when this node holds the whole table, or a
    /// leader declined and the read gathers records instead.
    ///
    /// The ceiling is on what travels: groups, never the records they fold
    /// (ADR-0097 D3).
    pub(crate) fn gather_folded(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        reduce: &crate::Reduce,
        select: &tessari_ql::Select,
        condition: Option<&tessari_ql::Expr>,
        noticed: &Noticed,
    ) -> Result<Option<(Groups, Note)>> {
        let Some(missing) = self.missing(transaction, id, Part::Whole)? else {
            return Ok(None);
        };
        let (Some(gatherer), Some(map)) = (self.gather.as_ref(), missing.map.as_ref()) else {
            return Err(missing.refusal());
        };
        let occurrences = occurrences(select.projection.written());
        let mut groups = Groups::new();
        for span in map.spans().filter(|span| missing.needed.contains(&span.id)) {
            let Some(window) = window_of(&span, Part::Whole) else {
                continue;
            };
            if missing.lacking.contains(&span.id) {
                let asked = Asked {
                    namespace: missing.namespace,
                    database: missing.database,
                    table: id,
                    shard: span.id,
                    window,
                    most: GATHER_RECORDS,
                    pushed: None,
                    enough: None,
                    reduce: Some(reduce),
                };
                let partials = match gatherer.gather(&asked) {
                    Ok(Gathered {
                        reduced: Some(crate::Reduced::Partials(partials)),
                        ..
                    }) => partials,
                    Ok(_) => return Ok(None),
                    Err(why) => return Err(missing.unanswered(span.id, why)),
                };
                if !merge_partials(&mut groups, &occurrences, partials)? {
                    return Ok(None);
                }
            } else {
                // This node's own span, a page at a time, tested here exactly as
                // a local read tests it — notes and refusals included.
                let mut after: Option<RecordId> = None;
                loop {
                    let page = transaction.records_between(
                        missing.namespace,
                        missing.database,
                        id,
                        window,
                        after.as_ref(),
                        GATHER_PAGE_RECORDS,
                    )?;
                    let full = page.len() == GATHER_PAGE_RECORDS;
                    after = page.last().map(|(id, _)| id.clone());
                    let mut kept = Vec::with_capacity(page.len());
                    for (record_id, record) in self.records_of(page, &reduce.visible)? {
                        if let Some(condition) = condition {
                            let held = self.evaluate_in(
                                transaction,
                                condition,
                                Scope::of(&record).identified(&record_id).noticing(noticed),
                            )?;
                            if !boolean(&held, condition.span)? {
                                continue;
                            }
                        }
                        kept.push((record_id, record));
                    }
                    if !self.fold_into(
                        transaction,
                        &mut groups,
                        kept,
                        &occurrences,
                        &select.group,
                        true,
                    )? {
                        return Ok(None);
                    }
                    if !full {
                        break;
                    }
                }
            }
            if groups.len() > GATHER_RECORDS {
                return Err(missing.too_much());
            }
        }
        Ok(Some((groups, missing.note())))
    }
}

/// The part of `span` that `part` needs, or `None` when they do not meet.
pub(crate) fn window_of<'a>(span: &ShardSpan<'a>, part: Part<'a>) -> Option<Window<'a>> {
    let inside =
        |id: &RecordId| span.from.is_none_or(|from| from <= id) && span.to.is_none_or(|to| id < to);
    match part {
        Part::Whole => Some(Window {
            from: span.from,
            to: span.to.map(|to| (to, false)),
        }),
        Part::Record(id) => inside(id).then_some(Window {
            from: Some(id),
            to: Some((id, true)),
        }),
        Part::Span {
            lower,
            upper,
            inclusive,
        } => {
            let empty = if inclusive {
                lower > upper
            } else {
                lower >= upper
            };
            if empty {
                return None;
            }
            let from = match span.from {
                Some(from) if from > lower => from,
                _ => lower,
            };
            // The span's own end is exclusive; the read's is whatever it said.
            let to = match span.to {
                Some(to) if to < upper || (to == upper && inclusive) => (to, false),
                _ => (upper, inclusive),
            };
            let reaches = if to.1 { from <= to.0 } else { from < to.0 };
            reaches.then_some(Window {
                from: Some(from),
                to: Some(to),
            })
        }
    }
}
