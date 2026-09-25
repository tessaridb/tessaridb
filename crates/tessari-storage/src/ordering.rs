//! One writer's logs applied in the order it committed them (G034, ADR-0084).
//!
//! # Why a follower cannot apply a log at a time
//!
//! A writer files each commit in the log of that commit's home, so its one
//! order is spread over several logs. Applying them one after another, each to
//! its tail, applies a later commit filed in a coarser log before an earlier one
//! filed in a finer log, and a record both touched ends at the OLDER value on
//! the follower with nothing in an error state (Q-796).
//!
//! # What a page proves, and why nothing past it is applied
//!
//! Every record carries its writer's order. A round fetches a page of each log
//! and applies the fetched records in that order — but only up to the order
//! every page proves complete. A full page proves its log up to its last
//! record; a level page proves its log up to the order the leader had reached
//! when it answered. A record past the smallest of those might be preceded by a
//! commit in a log fetched before it was written, so it waits for the next
//! round, which fetches again. Nothing is held between rounds.
//!
//! # A record without an order
//!
//! Written before commits carried one. It sorts first, and among such records
//! the round keeps collection order, which is what applying a log at a time
//! always did. A leader that states no order proves nothing about a level page,
//! so it does not bound the round.

use std::collections::BTreeMap;

use tessari_encoding::{LogId, LogRecord, Writer};
use tessari_types::{Epoch, Sequence};

use crate::{Change, Result, Store, Subject};
use tessari_constants::HISTORY_SCAN_RECORDS;

/// How much of its log one fetched page proves is all there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Horizon {
    /// The leader had no more, and had committed up to this order when it
    /// answered.
    Level(Sequence),
    /// The leader had no more and stated no order — one that predates it.
    Unstated,
    /// The page was full: more may follow its last record.
    Full,
}

/// One log's page as a round fetched it.
#[derive(Debug, Clone, Copy)]
pub struct Page<'a> {
    /// The log the records were read out of, which names their writer.
    pub log: LogId,
    /// The leadership that wrote the record before the first one here.
    pub previous: Epoch,
    /// The records, in log order.
    pub records: &'a [(Sequence, LogRecord)],
    /// What the page proves.
    pub horizon: Horizon,
}

/// Which fetched records to apply this round, in the order to apply them, as
/// `(page, position in that page)`.
///
/// What is chosen from each page is a prefix of it, so each log still applies
/// gap-free from its cursor. Pages of different writers are bounded
/// separately: two writers' orders are unrelated counters.
#[must_use]
pub fn in_writer_order(pages: &[Page<'_>]) -> Vec<(usize, usize)> {
    let mut bound: BTreeMap<Writer, Sequence> = BTreeMap::new();
    for page in pages {
        let proves = match page.horizon {
            Horizon::Level(order) => order,
            Horizon::Unstated => Sequence::new(u64::MAX),
            // A full page with nothing in it proves nothing.
            Horizon::Full => page
                .records
                .last()
                .map_or(Sequence::ZERO, |(_, record)| order_of(record)),
        };
        bound
            .entry(page.log.writer)
            .and_modify(|held| *held = (*held).min(proves))
            .or_insert(proves);
    }
    let mut chosen: Vec<(Sequence, usize, usize)> = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        let horizon = bound
            .get(&page.log.writer)
            .copied()
            .unwrap_or(Sequence::ZERO);
        for (position, (_, record)) in page.records.iter().enumerate() {
            let order = order_of(record);
            if order > horizon {
                break;
            }
            chosen.push((order, index, position));
        }
    }
    chosen.sort_unstable();
    chosen
        .into_iter()
        .map(|(_, index, position)| (index, position))
        .collect()
}

impl Store {
    /// Apply one round's pages in their writers' order, and answer, per page,
    /// the last position applied — `None` where nothing was.
    ///
    /// The one apply a follower's round goes through, so the collector and
    /// anything standing in for it apply by the same rule (ADR-0084). Each
    /// page's chosen records are a prefix of it, applied in position order, so
    /// each record is preceded by the one applied before it in its own log.
    ///
    /// # Errors
    ///
    /// Returns the store's refusal of the first record that does not apply;
    /// what was applied before it stays applied, as a log at a time did.
    pub fn apply_in_writer_order(&self, pages: &[Page<'_>]) -> Result<Vec<Option<Sequence>>> {
        let mut previous: Vec<Epoch> = pages.iter().map(|page| page.previous).collect();
        let mut reached: Vec<Option<Sequence>> = vec![None; pages.len()];
        for (index, position) in in_writer_order(pages) {
            let (Some(page), Some(before)) = (pages.get(index), previous.get_mut(index)) else {
                continue;
            };
            let Some((at, record)) = page.records.get(position) else {
                continue;
            };
            self.apply_from_stream(page.log, *at, *before, record)?;
            *before = record.epoch();
            if let Some(slot) = reached.get_mut(index) {
                *slot = Some(*at);
            }
        }
        Ok(reached)
    }
}

/// One record's history read out of several of one writer's logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedHistory {
    /// The events found, newest first by the writer's order, each with the log
    /// it was read out of — a position means something only in its own log.
    pub events: Vec<(LogId, Change)>,
    /// Whether every log was walked to its beginning. When one was not, the
    /// events stop where that log's walk stopped, so the answer has no hole.
    pub complete: bool,
    /// How many log records were read, over every log.
    pub walked: usize,
}

impl Store {
    /// One record's history out of `logs`, merged by the writer's order
    /// (G034, ADR-0084) — a split table's record is written in its shard's log
    /// by a commit touching one shard and in its database's by one touching two.
    ///
    /// Each log is walked newest first up to the same budget a single-log
    /// history uses. A log that hits it may hold older events below, so every
    /// event older than the oldest record that log was walked to is dropped
    /// rather than shown beside a gap, and the answer says it is not complete.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    pub fn history_across(
        &self,
        logs: &[LogId],
        subject: &Subject,
        limit: usize,
    ) -> Result<MergedHistory> {
        let mut found: Vec<(Sequence, LogId, Change)> = Vec::new();
        let mut floor = Sequence::ZERO;
        let mut complete = true;
        let mut walked = 0_usize;
        for log in logs {
            let records = self.log_records_newest_first(*log, HISTORY_SCAN_RECORDS)?;
            walked = walked.saturating_add(records.len());
            if records.len() >= HISTORY_SCAN_RECORDS || self.log_start(*log)?.get() > 1 {
                complete = false;
                if let Some((_, oldest)) = records.last() {
                    floor = floor.max(order_of(oldest));
                }
            }
            for (sequence, record) in &records {
                for change in crate::feed::changes_in(*sequence, record)? {
                    if subject.covers(&change) {
                        found.push((order_of(record), *log, change));
                    }
                }
            }
        }
        found.retain(|(order, _, _)| *order >= floor);
        found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        found.truncate(limit);
        Ok(MergedHistory {
            events: found
                .into_iter()
                .map(|(_, log, change)| (log, change))
                .collect(),
            complete,
            walked,
        })
    }
}

/// A record's order, with a record that carries none sorting first.
fn order_of(record: &LogRecord) -> Sequence {
    record.order().unwrap_or(Sequence::ZERO)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tessari_types::{DatabaseId, Epoch, NamespaceId, Reach};

    use super::*;

    const WRITER: Writer = Writer::new([1; 16]);

    fn log(home: Reach) -> LogId {
        LogId::new(home, WRITER)
    }

    fn database() -> LogId {
        log(Reach::Database(NamespaceId::new(1), DatabaseId::new(1)))
    }

    fn namespace() -> LogId {
        log(Reach::Namespace(NamespaceId::new(1)))
    }

    /// Records at positions 1.. carrying the given orders.
    fn records(orders: &[u64]) -> Vec<(Sequence, LogRecord)> {
        orders
            .iter()
            .zip(1_u64..)
            .map(|(order, at)| {
                let mut record = LogRecord::at(Epoch::new(1), Vec::new());
                record.set_order(Sequence::new(*order));
                (Sequence::new(at), record)
            })
            .collect()
    }

    #[test]
    fn records_of_two_logs_are_applied_interleaved_in_the_writers_order() {
        let finer = records(&[2, 4]);
        let coarser = records(&[3]);
        let pages = [
            Page {
                previous: Epoch::ZERO,
                log: namespace(),
                records: &coarser,
                horizon: Horizon::Level(Sequence::new(10)),
            },
            Page {
                previous: Epoch::ZERO,
                log: database(),
                records: &finer,
                horizon: Horizon::Level(Sequence::new(10)),
            },
        ];
        assert_eq!(in_writer_order(&pages), vec![(1, 0), (0, 0), (1, 1)]);
    }

    /// The case a log at a time gets wrong, and the case the horizon exists
    /// for: the namespace page was fetched AFTER the database page, and holds a
    /// commit (order 7) that may follow one written to the database log after
    /// the database page was read (order 6, not in the page). The database
    /// page's level proves only up to 5, so 7 waits.
    #[test]
    fn a_record_past_what_every_page_proves_waits_for_the_next_round() {
        let finer = records(&[3]);
        let coarser = records(&[7]);
        let pages = [
            Page {
                previous: Epoch::ZERO,
                log: database(),
                records: &finer,
                horizon: Horizon::Level(Sequence::new(5)),
            },
            Page {
                previous: Epoch::ZERO,
                log: namespace(),
                records: &coarser,
                horizon: Horizon::Level(Sequence::new(9)),
            },
        ];
        assert_eq!(in_writer_order(&pages), vec![(0, 0)]);
    }

    #[test]
    fn a_full_page_proves_its_log_only_up_to_its_last_record() {
        let full = records(&[2, 8]);
        let level = records(&[5, 9]);
        let pages = [
            Page {
                previous: Epoch::ZERO,
                log: database(),
                records: &full,
                horizon: Horizon::Full,
            },
            Page {
                previous: Epoch::ZERO,
                log: namespace(),
                records: &level,
                horizon: Horizon::Level(Sequence::new(10)),
            },
        ];
        assert_eq!(in_writer_order(&pages), vec![(0, 0), (1, 0), (0, 1)]);
    }

    /// Progress: the page that bounds the round is always applied whole when
    /// it is full, so a round never stalls on a log that holds records.
    #[test]
    fn the_page_that_bounds_a_round_is_applied_whole() {
        let full = records(&[4, 6]);
        let later = records(&[7, 8]);
        let pages = [
            Page {
                previous: Epoch::ZERO,
                log: database(),
                records: &full,
                horizon: Horizon::Full,
            },
            Page {
                previous: Epoch::ZERO,
                log: namespace(),
                records: &later,
                horizon: Horizon::Full,
            },
        ];
        assert_eq!(in_writer_order(&pages), vec![(0, 0), (0, 1)]);
    }

    #[test]
    fn records_without_an_order_keep_collection_order_as_before() {
        let unordered = |count: u64| -> Vec<(Sequence, LogRecord)> {
            (1..=count)
                .map(|at| (Sequence::new(at), LogRecord::at(Epoch::new(1), Vec::new())))
                .collect()
        };
        let first = unordered(2);
        let second = unordered(1);
        let pages = [
            Page {
                previous: Epoch::ZERO,
                log: namespace(),
                records: &first,
                horizon: Horizon::Unstated,
            },
            Page {
                previous: Epoch::ZERO,
                log: database(),
                records: &second,
                horizon: Horizon::Unstated,
            },
        ];
        assert_eq!(in_writer_order(&pages), vec![(0, 0), (0, 1), (1, 0)]);
    }

    #[test]
    fn two_writers_are_bounded_each_by_their_own_pages() {
        let other = Writer::new([2; 16]);
        let mine = records(&[3, 50]);
        let theirs = records(&[1]);
        let pages = [
            Page {
                previous: Epoch::ZERO,
                log: database(),
                records: &mine,
                horizon: Horizon::Level(Sequence::new(60)),
            },
            Page {
                previous: Epoch::ZERO,
                log: LogId::new(Reach::Store, other),
                records: &theirs,
                horizon: Horizon::Level(Sequence::new(1)),
            },
        ];
        // The other writer's small bound does not hold this writer's 50 back.
        assert_eq!(in_writer_order(&pages), vec![(1, 0), (0, 0), (0, 1)]);
    }
}
