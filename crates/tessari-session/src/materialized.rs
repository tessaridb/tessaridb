//! Materialized views: a read's answer kept as records and brought current from
//! the change feed (ADR-0109).
//!
//! # Recomputed, never patched
//!
//! A batch never adds a delta to a stored row. It finds which part of the view
//! a batch of source changes can have touched — the changed records for a view
//! that answers one row per record, the whole view otherwise — and asks the
//! ordinary read engine for that part again, in the same transaction that
//! writes it. The rows are therefore what `SELECT … VERSION s` answers at the
//! version the batch states, by construction: there is no second evaluator to
//! drift from the first. A change delivered twice is recomputed twice, at a
//! newer snapshot, and changes nothing it should not.
//!
//! # Where the batch's snapshot bounds it
//!
//! The batch opens its transaction first and reads the merged feed of the
//! source's logs **bounded at that snapshot**, to the end. A change past the
//! snapshot waits for the next batch rather than being consumed by this one: a
//! row recomputed at the snapshot does not reflect it, so consuming it here
//! would lose it.

mod shape;

use std::collections::BTreeSet;

use tessari_encoding::encode_payload;
use tessari_ql::{Identity, RecordTarget, Source};
use tessari_storage::{
    Catalog, Merged, RecordAddress, Store, TableKind, Transaction, ViewState, Watch,
};
use tessari_types::{RecordId, Sequence, TableId};

use crate::condition::boolean;
use crate::context::Context;
use crate::error::Result;
use crate::evaluate::Scope;
use crate::session::Session;

pub(crate) use shape::{Shape, Understood};

/// How many changes one read of the feed asks for; a batch reads until the
/// snapshot is reached.
const FEED_PAGE: usize = 1_000;

/// How many changed records a per-record view recomputes one by one before a
/// batch recomputes it whole instead — past this a single read of the view is
/// cheaper than as many record reads.
const RECOMPUTE_WHOLE_PAST: usize = 10_000;

/// How long an idle view's state may go unwritten before a batch advances its
/// stated version anyway, so its reported lag measures change rather than
/// silence. Milliseconds.
const IDLE_REFRESH_MILLIS: i64 = 10_000;

/// What one materialized view is, resolved for a batch.
pub(crate) struct Kept {
    /// The view's own tenancy and table.
    pub(crate) context: Context,
    pub(crate) view: TableId,
    /// Its read, understood.
    pub(crate) understood: Understood,
    /// The source table's id.
    pub(crate) source: TableId,
}

/// What a maintenance pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Maintained {
    /// Views whose rows changed.
    pub views: usize,
    /// Source changes applied across them.
    pub changes: usize,
}

impl Session<'_> {
    /// `DEFINE VIEW … MATERIALIZED AS …`: judge the read, declare the view, and
    /// fill it in the same transaction, so a view is never readable before it
    /// holds its rows (ADR-0109 D3).
    pub(crate) fn define_materialized(
        &self,
        transaction: &mut Transaction<'_>,
        name: &tessari_ql::Name,
        read: &str,
        if_not_exists: bool,
        span: tessari_ql::Span,
    ) -> Result<crate::outcome::Outcome> {
        let understood = Understood::of(read, span)?;
        // The source must be a table this session may read now: a view, a
        // vault or a missing name is refused with its own message.
        let (_, source) = self.resolve_table(transaction, &understood.source)?;
        self.refuse_reading_a_vault(transaction, source, &understood.source)?;
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .table_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(crate::outcome::Outcome::Done);
        }
        let outcome = self.define_table(
            transaction,
            name,
            tessari_storage::TableShape {
                schemafull: false,
                kind: TableKind::View(tessari_storage::ViewDeclaration {
                    read: read.to_owned(),
                    materialized: true,
                }),
                identity: tessari_types::IdentityKind::default(),
                graph: None,
                conflict: None,
                split: Vec::new(),
                partition: None,
                spread: false,
            },
            false,
            span,
        )?;
        let Some(view) =
            Catalog::new(transaction).table_id(context.namespace, context.database, &name.text)?
        else {
            return Ok(outcome);
        };
        self.build_view(
            transaction,
            &Kept {
                context,
                view,
                understood,
                source,
            },
        )?;
        Ok(outcome)
    }

    /// Fill a just-declared materialized view in the declaring transaction:
    /// its whole read, and a state stating the transaction's snapshot.
    pub(crate) fn build_view(&self, transaction: &mut Transaction<'_>, kept: &Kept) -> Result<()> {
        self.recompute_whole(transaction, kept)?;
        let snapshot = transaction.snapshot();
        let positions = self.positions_after(kept, snapshot, &[])?;
        transaction.put_view_state(
            kept.view,
            &ViewState {
                version: snapshot,
                positions,
                refreshed: now_millis(),
            },
        )?;
        Ok(())
    }

    /// Bring one materialized view current to `transaction`'s snapshot.
    ///
    /// Answers how many source changes it applied, or `None` when nothing was
    /// written — no change, and a state recent enough to leave alone.
    pub(crate) fn maintain_view(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
    ) -> Result<Option<usize>> {
        let snapshot = transaction.snapshot();
        let Some(state) = transaction.view_state(kept.view)? else {
            // A view without a state is one this build did not fill: fill it.
            self.build_view(transaction, kept)?;
            return Ok(Some(0));
        };
        let positions = self.positions_after(kept, state.version, &state.positions)?;
        let mut feed = Merged::new(positions, Watch::table(kept.source));
        let mut changed: BTreeSet<RecordId> = BTreeSet::new();
        let mut applied = 0_usize;
        let mut whole = !matches!(kept.understood.shape, Shape::PerRecord);
        loop {
            // A page can hold nothing of the source and still be followed by
            // more, so the end is where the feed stops moving, not an empty page.
            let before = feed.positions().to_vec();
            let page = feed.poll_until(self.store, FEED_PAGE, snapshot)?;
            for (_, change) in page {
                if change.namespace != kept.context.namespace
                    || change.database != kept.context.database
                {
                    continue;
                }
                applied = applied.saturating_add(1);
                if !whole {
                    changed.insert(change.id);
                    whole = changed.len() > RECOMPUTE_WHOLE_PAST;
                }
            }
            if feed.positions() == before.as_slice() {
                break;
            }
        }
        let now = now_millis();
        if applied == 0 && now.saturating_sub(state.refreshed) < IDLE_REFRESH_MILLIS {
            return Ok(None);
        }
        if applied > 0 {
            if whole {
                self.recompute_whole(transaction, kept)?;
            } else {
                for id in &changed {
                    self.recompute_record(transaction, kept, id)?;
                }
            }
        }
        transaction.put_view_state(
            kept.view,
            &ViewState {
                version: snapshot,
                positions: feed.positions().to_vec(),
                refreshed: now,
            },
        )?;
        Ok(Some(applied))
    }

    /// Where to read each log from: a log the state names keeps its position,
    /// and one it does not — new since — starts after the stated version.
    fn positions_after(
        &self,
        kept: &Kept,
        version: Sequence,
        held: &[(tessari_encoding::LogId, Sequence)],
    ) -> Result<Vec<(tessari_encoding::LogId, Sequence)>> {
        let store = self.store;
        let mut positions = Vec::new();
        for log in
            store.logs_carrying(kept.context.namespace, kept.context.database, kept.source)?
        {
            let at = match held.iter().find(|(known, _)| *known == log) {
                Some((_, at)) => *at,
                None => store.first_after(log, version)?,
            };
            positions.push((log, at));
        }
        Ok(positions)
    }

    /// Replace every stored row with the read's whole answer.
    fn recompute_whole(&self, transaction: &mut Transaction<'_>, kept: &Kept) -> Result<()> {
        let stored =
            transaction.scan_table(kept.context.namespace, kept.context.database, kept.view)?;
        for (id, _) in stored {
            transaction.delete(self.row(kept, id));
        }
        let answered = self.read(transaction, &kept.understood.select, None, None)?;
        let keyed = matches!(kept.understood.shape, Shape::PerRecord);
        for (position, (id, value)) in answered.records.into_iter().enumerate() {
            // A per-record view keeps the source record's identity, so its rows
            // read back in the order the read answers them; any other view keeps
            // the answer's position, which is the order its read declared.
            let id = if keyed {
                id
            } else {
                RecordId::Int(i64::try_from(position).unwrap_or(i64::MAX))
            };
            transaction.put(self.row(kept, id), encode_payload(&value).into_bytes());
        }
        Ok(())
    }

    /// Recompute the one row a source record answers, or remove it.
    fn recompute_record(
        &self,
        transaction: &mut Transaction<'_>,
        kept: &Kept,
        id: &RecordId,
    ) -> Result<()> {
        let source = RecordAddress::new(
            kept.context.namespace,
            kept.context.database,
            kept.source,
            id.clone(),
        );
        let passes = match (transaction.get(&source)?, &kept.understood.condition) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(payload), Some(condition)) => {
                let record = self.record_of(&payload, &None)?;
                let searched = self.searched_for(transaction, kept.source, &[condition])?;
                let value = self.evaluate_in(
                    transaction,
                    condition,
                    Scope::searching(&record, &searched).identified(id),
                )?;
                boolean(&value, condition.span)?
            }
        };
        let row = self.row(kept, id.clone());
        if !passes {
            transaction.delete(row);
            return Ok(());
        }
        // The projection is the read's own, over this one record.
        let mut one = kept.understood.select.clone();
        one.from = Source::Record(RecordTarget {
            table: kept.understood.source.clone(),
            id: Identity::Fixed(id.clone()),
            span: kept.understood.source.span,
        });
        let answered = self.read(transaction, &one, None, None)?;
        match answered.records.into_iter().next() {
            Some((_, value)) => transaction.put(row, encode_payload(&value).into_bytes()),
            None => transaction.delete(row),
        }
        Ok(())
    }

    fn row(&self, kept: &Kept, id: RecordId) -> RecordAddress {
        RecordAddress::new(kept.context.namespace, kept.context.database, kept.view, id)
    }

    /// The materialized view `view` names, resolved: its read understood and
    /// its source found in the view's own tenancy.
    pub(crate) fn materialized_view(
        &self,
        transaction: &mut Transaction<'_>,
        view: TableId,
    ) -> Result<Option<Kept>> {
        let Some(definition) = Catalog::new(transaction).table(view)? else {
            return Ok(None);
        };
        let TableKind::View(declared) = &definition.kind else {
            return Ok(None);
        };
        if !declared.materialized {
            return Ok(None);
        }
        let understood = Understood::of(&declared.read, tessari_ql::Span::new(0, 0))?;
        let context = Context {
            namespace: definition.namespace,
            database: definition.database,
        };
        let Some(source) = Catalog::new(transaction).table_id(
            context.namespace,
            context.database,
            &understood.source.name.text,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Kept {
            context,
            view,
            understood,
            source,
        }))
    }
}

/// Bring every materialized view on this node current, one transaction per
/// view (ADR-0109 D6).
///
/// A view this node may not write is refused at its commit, before anything
/// changes, and left to the node that may.
///
/// # Errors
///
/// Returns an error when the catalog cannot be read, or a view's maintenance
/// fails for a reason other than not being this node's to write.
pub fn maintain_views(store: &Store) -> Result<Maintained> {
    let views = {
        let mut transaction = store.begin()?;
        let found = materialized_views(&mut transaction)?;
        transaction.rollback();
        found
    };
    let mut done = Maintained::default();
    for (view, namespace, database) in views {
        // The view's read names its source as its author wrote it, unqualified,
        // so the session selects the view's own tenancy — as the author's did.
        let mut session = Session::new(store);
        session.namespace = Some(namespace);
        session.database = Some(database);
        let mut transaction = store.begin()?;
        let Some(kept) = session.materialized_view(&mut transaction, view)? else {
            transaction.rollback();
            continue;
        };
        match session.maintain_view(&mut transaction, &kept)? {
            Some(applied) => {
                transaction.commit()?;
                if applied > 0 {
                    done.views = done.views.saturating_add(1);
                    done.changes = done.changes.saturating_add(applied);
                }
            }
            None => transaction.rollback(),
        }
    }
    Ok(done)
}

/// Every materialized view in the store, with the names of its tenancy.
fn materialized_views(transaction: &mut Transaction<'_>) -> Result<Vec<(TableId, String, String)>> {
    let mut found = Vec::new();
    let catalog = Catalog::new(transaction);
    for namespace in catalog.namespaces()? {
        for database in catalog.databases_in(namespace.id)? {
            for table in catalog.tables_in(namespace.id, database.id)? {
                if matches!(&table.kind, TableKind::View(declared) if declared.materialized) {
                    found.push((table.id, namespace.name.clone(), database.name.clone()));
                }
            }
        }
    }
    Ok(found)
}

/// Now, as milliseconds since the Unix epoch.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The version a materialized view's rows equal its read at, and how far the
/// store has moved past it — what `INFO FOR TABLE` reports.
pub(crate) fn freshness(
    store: &Store,
    transaction: &Transaction<'_>,
    view: TableId,
) -> Result<Option<(ViewState, Sequence)>> {
    let Some(state) = transaction.view_state(view)? else {
        return Ok(None);
    };
    let head = store.committed_version()?;
    Ok(Some((state, head)))
}
