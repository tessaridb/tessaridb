//! The initial scan: existing rows of a fed table, carried into the topic
//! by a background job the feed owns (FEAT-S1 D8).
//!
//! # The snapshot is the define commit's
//!
//! Live capture switches on at the define commit, so the scan must anchor
//! exactly there: a record whose newest version is at or before the snapshot
//! has no live message and the scan carries it; a record written later has
//! one and the scan skips it. One message per key, no gap, no duplicate. The
//! job state lives in a system row written by the defining commit itself, so
//! the row's key version IS the snapshot and there is no crash window
//! between the declaration and its anchor; the first tick resolves it, and
//! from then on the stored value governs across restarts.
//!
//! # As whom
//!
//! The scan is engine work, like the reaper: there is no writer whose
//! authority could decide the append, and none is claimed. The images it
//! emits are engine-visible — the stored record as it stands — where the
//! live capture redacts to what the writer may read; the declaration's
//! image mode and `WHEN` are honoured either way.

use tessari_ql::{Parameters, Script, Span, Statement, StatementKind};
use tessari_storage::{Catalog, FeedDeclaration, FeedScanState, Store, Transaction};
use tessari_types::{DatabaseId, NamespaceId, Sequence, TableId, Value, WriteKind};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::session::Session;

/// What one cadence tick of the scan job did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scanned {
    /// Feeds that took a step (a batch examined and committed).
    pub feeds: usize,
    /// Records emitted across those feeds.
    pub records: usize,
}

/// Advance every feed's initial scan by one bounded batch — the function the
/// node's housekeeping cadence calls.
///
/// One transaction per feed: the batch's messages, and the job state
/// describing them, land in one commit or neither. A feed this node may not
/// write is refused at its commit like any write, and left to the node that
/// may; a feed whose job is done or was dropped with the feed is skipped.
///
/// # Errors
///
/// Returns the first failure that is not "not this node's to write".
pub async fn scan_feeds(store: &Store) -> Result<Scanned> {
    let pending = {
        let mut transaction = store.begin().await?;
        let found = pending_scans(&mut transaction).await?;
        transaction.rollback();
        found
    };
    let mut done = Scanned::default();
    for (namespace, database, table, definition, feed) in pending {
        if scan_one(store, namespace, database, table, &definition, &feed)
            .await?
            .is_some()
        {
            done.feeds = done.feeds.saturating_add(1);
        }
    }
    Ok(done)
}

/// Every feed carrying `WITH INITIAL SCAN` whose job is not done: the
/// tenancy ids, the table's name, and the declaration.
async fn pending_scans(
    transaction: &mut Transaction<'_>,
) -> Result<Vec<(NamespaceId, DatabaseId, TableId, String, FeedDeclaration)>> {
    let mut candidates: Vec<(NamespaceId, DatabaseId, tessari_storage::TableDefinition)> =
        Vec::new();
    {
        let catalog = Catalog::new(transaction);
        for namespace in catalog.namespaces().await? {
            for database in catalog.databases_in(namespace.id).await? {
                for table in catalog.tables_in(namespace.id, database.id).await? {
                    candidates.push((namespace.id, database.id, table));
                }
            }
        }
    }
    let mut found = Vec::new();
    for (namespace, database, table) in candidates {
        for feed in &table.feeds {
            if !feed.initial_scan {
                continue;
            }
            let at = transaction.feed_scan_state(table.id, &feed.name).await?;
            if at.is_some_and(|state| state.done) {
                continue;
            }
            found.push((
                namespace,
                database,
                table.id,
                table.name.clone(),
                feed.clone(),
            ));
        }
    }
    Ok(found)
}

/// One feed's step: resolve the snapshot if the job is new, examine one
/// batch, emit the rows it owes, and carry the job state forward in the same
/// commit. Answers `None` when there is nothing to do — the feed was dropped
/// between the enumeration and this transaction, or a racing node finished
/// the job first.
///
/// # Errors
///
/// Returns an error when the batch cannot be read or a message cannot land.
async fn scan_one(
    store: &Store,
    namespace: NamespaceId,
    database: DatabaseId,
    table: TableId,
    table_name: &str,
    feed: &FeedDeclaration,
) -> Result<Option<usize>> {
    let mut transaction = store.begin().await?;
    let Some(state) = transaction.feed_scan_state(table, &feed.name).await? else {
        transaction.rollback();
        return Ok(None);
    };
    if state.done {
        transaction.rollback();
        return Ok(None);
    }
    let snapshot = if state.snapshot == 0 {
        store.feed_scan_defined_at(table, &feed.name).await?
    } else {
        Sequence::new(state.snapshot)
    };
    let batch = store
        .scan_feed_batch(namespace, database, table, state.next.as_deref(), snapshot)
        .await?;
    let mut session = Session::new(store);
    let context = Context {
        namespace,
        database,
    };
    let mut emitted = 0_usize;
    for (id, after) in &batch.records {
        session
            .append_scan(&mut transaction, &context, feed, id, after, table_name)
            .await?;
        emitted = emitted.saturating_add(1);
    }
    transaction.put_feed_scan_state(
        table,
        &feed.name,
        &FeedScanState {
            snapshot: snapshot.get(),
            next: batch.resume.clone(),
            done: batch.resume.is_none(),
        },
    )?;
    transaction.commit().await?;
    Ok(Some(emitted))
}

impl<'a> Session<'a> {
    /// One scanned row's envelope, appended to its feed's topic.
    ///
    /// Engine work, so no caller funnel and no writer-authorization probe:
    /// the declaration's `WHEN` and image mode are honoured, the partition
    /// field routes the message as the live capture's does, and the append
    /// lands in the scan's own transaction.
    ///
    /// # Errors
    ///
    /// [`Error::FeedFailed`] carrying whatever the append refused.
    pub(crate) async fn append_scan(
        &mut self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        feed: &FeedDeclaration,
        id: &tessari_types::RecordId,
        after: &Value,
        table: &str,
    ) -> Result<()> {
        let bindings = Parameters::from([
            ("event".to_owned(), Value::from(WriteKind::Create.word())),
            ("before".to_owned(), Value::None),
            ("after".to_owned(), after.clone()),
            ("id".to_owned(), crate::info::id_value(id.clone())),
        ]);
        if let Some(when) = &feed.when {
            let condition = Script {
                statements: vec![Statement {
                    kind: StatementKind::Return {
                        value: crate::event::parsed::condition(when)?,
                    },
                    span: Span::new(0, 0),
                    acknowledge: None,
                    across: false,
                }],
                span: Span::new(0, 0),
            }
            .bind(&bindings)?;
            let Some(StatementKind::Return { value }) = condition
                .statements
                .first()
                .map(|statement| &statement.kind)
            else {
                return Ok(());
            };
            if self.evaluate(transaction, value).await? != Value::Bool(true) {
                return Ok(());
            }
        }
        let mut envelope = std::collections::BTreeMap::from([
            ("key".to_owned(), crate::info::id_value(id.clone())),
            ("op".to_owned(), Value::from(WriteKind::Create.word())),
            ("at".to_owned(), Value::Datetime(self.now_datetime())),
        ]);
        if feed.image == tessari_ql::FeedImage::Full {
            envelope.insert("after".to_owned(), after.clone());
        }
        let payload = Value::Object(envelope);

        let topic = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &feed.topic)
            .await?
            .ok_or_else(|| Error::FeedFailed {
                feed: feed.name.clone(),
                table: table.to_owned(),
                cause: Box::new(Error::Unknown {
                    entity: "topic",
                    name: feed.topic.clone(),
                    span: Span::new(0, 0),
                }),
            })?;
        let topic_definition = Catalog::new(transaction).table(topic).await?;
        let identity = self
            .free_identity(transaction, context, topic, &payload, Span::new(0, 0))
            .await
            .map_err(|cause| Error::FeedFailed {
                feed: feed.name.clone(),
                table: table.to_owned(),
                cause: Box::new(cause),
            })?;
        // A split topic's message lands in the partition its key chooses
        // (FEAT-S1 D9), exactly as the live capture's does.
        let home = self
            .partition_for(topic_definition.as_ref(), &payload, Span::new(0, 0))?
            .unwrap_or(topic);
        let at = tessari_storage::RecordAddress::new(
            context.namespace,
            context.database,
            home,
            identity,
        );
        // The engine's own write, beside the claim and the release: it does
        // not run the caller funnel, so it is not refused by the ownership it
        // exercises.
        self.put_engine_record(transaction, at, payload, Span::new(0, 0))
            .await
            .map_err(|cause| Error::FeedFailed {
                feed: feed.name.clone(),
                table: table.to_owned(),
                cause: Box::new(cause),
            })?;
        Ok(())
    }
}
