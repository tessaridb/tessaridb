//! Where a feed reads: one log, or a split scope's several (Q-791).
//!
//! A scope with no split table in it reads the database's log alone, exactly as
//! a feed always has. One with a split table reads that log and each of its
//! shards' logs, merged in the writer's order, and hands every change the cursor
//! to resume after it.

use std::collections::BTreeMap;

use tessari_storage::{Change, LogId, Merged, Subscription, Watch};
use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, ShardId, TableId};

use super::cursor;
use crate::Db;

/// A split table in the feed's scope: its name, its id and its shards.
pub(super) type Split = (String, TableId, Vec<ShardId>);

/// The logs a feed reads, and where it is in them.
pub(super) enum Source {
    /// The database's log, as a feed over no split table has always read it.
    One(Subscription),
    /// The database's log and its split tables' shard logs.
    Many {
        /// The cursor over them.
        merged: Merged,
        /// The tenancy the cursor names.
        tenancy: (NamespaceId, DatabaseId),
        /// Where the subscriber resumes, per home, after what it was given.
        at: BTreeMap<Reach, Sequence>,
    },
}

impl Source {
    /// The source for `tenancy` with `split` in scope.
    ///
    /// # Errors
    ///
    /// Names a cursor this feed cannot read, and a home holding another node's
    /// writes — one writer's order is the only one there is to merge by.
    pub(super) fn open(
        db: &Db,
        (namespace, database): (NamespaceId, DatabaseId),
        split: &[Split],
        from: Sequence,
        resume: Option<&str>,
        watch: Watch,
    ) -> Result<Self, String> {
        let store = db.store();
        let own = store.writer().map_err(|failure| failure.to_string())?;
        let whole = Reach::Database(namespace, database);
        if split.is_empty() {
            // One log, whose sequence is its position: a cursor here was carried
            // from some other feed, and ignoring it would resume from `from`
            // while the subscriber believes it resumed from the cursor.
            if let Some(text) = resume {
                return Err(format!(
                    "{text:?} is a cursor, and this feed follows no split table — resume it \
                     from the sequence of the last change handled"
                ));
            }
            let log = LogId::new(whole, own);
            return Ok(Self::One(Db::subscribe(log, from, watch)));
        }
        let mut homes = vec![(whole, None)];
        for (name, table, shards) in split {
            for shard in shards {
                homes.push((
                    Reach::Shard(namespace, database, *table, *shard),
                    Some((name, shard)),
                ));
            }
        }
        for (home, named) in &homes {
            let logs = store
                .logs_of(*home)
                .map_err(|failure| failure.to_string())?;
            if logs.iter().any(|log| log.writer != own) {
                let what = named.map_or_else(
                    || "this database's log".to_owned(),
                    |(name, shard)| format!("shard {shard} of `{name}`"),
                );
                return Err(format!(
                    "{what} holds another node's writes, and a change feed over a split \
                     table follows one writer's logs — follow it on the node that writes them"
                ));
            }
        }
        let held = match resume {
            Some(text) => cursor::read(text, namespace, database)?,
            None => BTreeMap::from([(whole, from)]),
        };
        // A log this feed does not read has no place in its cursor: it came from
        // a feed over another table, and resuming from it would be a guess.
        let stray = held
            .keys()
            .any(|home| !homes.iter().any(|(own, _)| own == home));
        if let (Some(text), true) = (resume, stray) {
            return Err(format!(
                "{text:?} counts a log this feed does not follow — send the cursor this \
                 feed's own last change carried"
            ));
        }
        // A home the cursor does not name is read from its beginning: its
        // changes were never given, so nothing is repeated and nothing lost.
        let at: BTreeMap<Reach, Sequence> = homes
            .iter()
            .map(|(home, _)| (*home, held.get(home).copied().unwrap_or(Sequence::ZERO)))
            .collect();
        let logs = at
            .iter()
            .map(|(home, position)| (LogId::new(*home, own), *position))
            .collect();
        Ok(Self::Many {
            merged: Merged::new(logs, watch),
            tenancy: (namespace, database),
            at,
        })
    }

    /// The next changes, each with the cursor to resume after it — `None` for a
    /// feed over one log, whose change's sequence is its cursor.
    ///
    /// # Errors
    ///
    /// Returns the store's failure, phrased for the subscriber.
    pub(super) fn next(
        &mut self,
        db: &Db,
        limit: usize,
    ) -> Result<Vec<(Change, Option<String>)>, String> {
        match self {
            Self::One(subscription) => Ok(db
                .poll(subscription, limit)
                .map_err(|failure| failure.to_string())?
                .into_iter()
                .map(|change| (change, None))
                .collect()),
            Self::Many {
                merged,
                tenancy: (namespace, database),
                at,
            } => {
                let found = merged
                    .poll(db.store(), limit)
                    .map_err(|failure| failure.to_string())?;
                Ok(found
                    .into_iter()
                    .map(|(log, change)| {
                        at.insert(
                            log.home,
                            Sequence::new(change.sequence.get().saturating_add(1)),
                        );
                        (change, Some(cursor::spell(*namespace, *database, at)))
                    })
                    .collect())
            }
        }
    }
}
