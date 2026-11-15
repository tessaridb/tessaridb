//! Feeds: the declared capture of a table's writes into a topic.
//!
//! # Where they run
//!
//! A write to a fed table stores the record's pre-transaction image in the
//! transaction the first time the record is touched (the capture lives on the
//! transaction because it is the one handle the write funnel and the commit
//! share, whatever session — a caller's or an event child's — the write ran
//! in). The COMMIT — explicit, a lone statement's own, or a `VERIFY`
//! rehearsal — drains the captures: the record's final buffered value decides
//! the net effect (`CREATE`/`UPDATE`/`DELETE`, or nothing when the record is
//! absent on both sides), the envelope is built, and it lands in the feed's
//! topic in the same commit as the writes it describes. The write and the
//! message land together or neither does, and a reader of the topic sees a
//! position only past the commit that took it.
//!
//! # As whom
//!
//! The append is authorized exactly as the writer's own, the way an event
//! body's statements are (ADR-0110 D4): a writer who may write the table but
//! not append the topic has the write refused, carrying the refusal. What the
//! envelope may carry is redacted to the fields the writer may read, for the
//! same reason an event's `$before`/`$after` are.
//!
//! # The topic is the feed's
//!
//! A topic named by `INTO` refuses hand appends; only this flush (and
//! retention) writes it, so a consumer cannot be fed forgeries it cannot tell
//! from captured writes.

use std::collections::BTreeMap;

use tessari_ql::{
    Answer, CreateTarget, Expr, ExprKind, FeedImage, Identity, Name, Parameters, RecordTarget,
    Script, Span, Statement, StatementKind, TableRef,
};
use tessari_storage::{Catalog, FeedDeclaration, TableKind, Transaction};
use tessari_types::{Datetime, Value, WriteKind};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// A `DEFINE FEED` statement's parts, as the parser left them.
pub(crate) struct DeclaredFeed<'s> {
    pub(crate) name: &'s Name,
    pub(crate) table: &'s TableRef,
    pub(crate) topic: &'s TableRef,
    pub(crate) on: &'s [WriteKind],
    pub(crate) image: FeedImage,
    pub(crate) when: Option<&'s String>,
    pub(crate) initial_scan: bool,
    pub(crate) if_not_exists: bool,
}

/// The word a refused kind is named by — the event table's, a feed being a
/// declaration on exactly the kinds an event is.
pub(crate) use super::event::kind_word;

impl<'a> Session<'a> {
    /// `DEFINE FEED`.
    ///
    /// # Errors
    ///
    /// [`Error::FeedOnKind`] on a table that cannot carry one,
    /// [`Error::FeedExists`] for a name already defined there,
    /// [`Error::TopicOwnedByFeed`] when the topic already belongs to a feed,
    /// [`Error::FeedTopicDeduplicated`] when the topic drops repeated keys —
    /// a dropped capture is a loss the feed's premise forbids — and whatever
    /// resolving the table or the topic refuses.
    pub(crate) async fn define_feed(
        &self,
        transaction: &mut Transaction<'_>,
        declared: &DeclaredFeed<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_any_table(transaction, declared.table).await?;
        let Some(definition) = Catalog::new(transaction).table(id).await? else {
            return Err(Error::Unknown {
                entity: "table",
                name: declared.table.name.text.clone(),
                span: declared.table.span,
            });
        };
        if let Some(kind) = kind_word(&definition.kind) {
            return Err(Error::FeedOnKind {
                table: definition.name.clone(),
                kind,
                span: declared.table.span,
            });
        }
        // The feed captures from the row authority, which a columnar
        // table fully retains, so the combination is designed — but the
        // projection's write-batch work and the capture's are not composed
        // in v1, and the refusal is named so a later wave lifts it with tests.
        if definition.storage_kind == Some(tessari_storage::StorageKind::Column) {
            return Err(Error::FeedOnColumnar {
                table: definition.name.clone(),
                span: declared.table.span,
            });
        }
        // The name is unique on the table, checked before the topic is
        // examined: a name already taken is refused on the table alone.
        if definition
            .feeds
            .iter()
            .any(|held| held.name == declared.name.text)
        {
            if declared.if_not_exists {
                return Ok(Outcome::Done);
            }
            return Err(Error::FeedExists {
                feed: declared.name.text.clone(),
                table: definition.name,
                span,
            });
        }
        // The topic is the feed's sink and must live in the table's database:
        // the capture appends in the writer's transaction, and a transaction
        // is one database's.
        let topic = Catalog::new(transaction)
            .table_id(
                definition.namespace,
                definition.database,
                &declared.topic.name.text,
            )
            .await?
            .ok_or_else(|| Error::Unknown {
                entity: "topic",
                name: declared.topic.name.text.clone(),
                span: declared.topic.span,
            })?;
        let Some(topic_definition) = Catalog::new(transaction).table(topic).await? else {
            return Err(Error::Unknown {
                entity: "topic",
                name: declared.topic.name.text.clone(),
                span: declared.topic.span,
            });
        };
        let TableKind::Topic(topic_kind) = &topic_definition.kind else {
            return Err(Error::NotATopic {
                table: declared.topic.name.text.clone(),
                span: declared.topic.span,
            });
        };
        if let Some(owner) = &topic_kind.owned_by {
            return Err(Error::TopicOwnedByFeed {
                topic: topic_definition.name,
                feed: owner.clone(),
                span: declared.topic.span,
            });
        }
        if topic_kind.deduplicate.is_some() {
            return Err(Error::FeedTopicDeduplicated {
                topic: topic_definition.name,
                span: declared.topic.span,
            });
        }
        // A partitioned topic's per-key order is a promise the captured
        // messages keep only if the record can carry the routing field (D9):
        // a schema-full table that does not declare it can never hold it, and
        // the feed would route by turn. A schemaless table may gain the field
        // on any record, so the define cannot know and is not refused.
        if let TableKind::Topic(tessari_storage::TopicDeclaration {
            partitions: Some(partitioning),
            ..
        }) = &topic_definition.kind
            && let Some(field) = &partitioning.by
            && definition.schemafull
            && !Catalog::new(transaction)
                .fields_on(definition.namespace, definition.database, id)
                .await?
                .iter()
                .any(|held| &held.name == field)
        {
            return Err(Error::FeedPartitionFieldMissing {
                field: field.clone(),
                table: definition.name.clone(),
                topic: topic_definition.name.clone(),
                span: declared.topic.span,
            });
        }
        transaction.guard_table_entry(id);
        transaction.guard_table_entry(topic);
        let mut feeds = definition.feeds;
        feeds.push(FeedDeclaration {
            name: declared.name.text.clone(),
            topic: declared.topic.name.text.clone(),
            on: declared.on.to_vec(),
            image: declared.image,
            when: declared.when.cloned(),
            initial_scan: declared.initial_scan,
        });
        feeds.sort_by(|left, right| left.name.cmp(&right.name));
        Catalog::new(transaction).set_feeds(id, feeds).await?;
        Catalog::new(transaction)
            .set_topic_owner(topic, Some(declared.name.text.clone()))
            .await?;
        // The initial scan's job state lands in the same commit, so the
        // row's key version is the define commit's sequence — the snapshot
        // the scan anchors at, with no second write to carry it.
        // The row names system table 31, a format-10 surface: a store the
        // build finalized to 9 cannot hold the job.
        if declared.initial_scan {
            self.refuse_a_format_the_store_does_not_hold(
                "a feed's initial scan",
                tessari_encoding::FormatVersion::FEED_SCAN,
            )
            .await?;
            transaction.put_feed_scan_state(
                id,
                &declared.name.text,
                &tessari_storage::FeedScanState::pending(),
            )?;
        }
        Ok(Outcome::Done)
    }

    /// `DROP FEED`: removes the capture and frees its topic for
    /// hand appends, in one catalog write each way.
    ///
    /// # Errors
    ///
    /// [`Error::Unknown`] naming the feed when the table has none by that
    /// name, and whatever resolving the table refuses.
    pub(crate) async fn drop_feed(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        table: &TableRef,
        span: Span,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_any_table(transaction, table).await?;
        let Some(definition) = Catalog::new(transaction).table(id).await? else {
            return Err(Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            });
        };
        transaction.guard_table_entry(id);
        let Some(held) = definition
            .feeds
            .iter()
            .find(|held| held.name == name.text)
            .cloned()
        else {
            return Err(Error::Unknown {
                entity: "feed",
                name: name.text.clone(),
                span,
            });
        };
        let mut feeds = definition.feeds;
        feeds.retain(|held| held.name != name.text);
        Catalog::new(transaction).set_feeds(id, feeds).await?;
        // The scan job dies with the feed: a redefine starts a fresh one.
        transaction.forget_feed_scan(id, &name.text);
        // The topic is the feed's no longer: a caller may append to it again.
        // Resolved in the table's database, where the define resolved it.
        if let Some(topic) = Catalog::new(transaction)
            .table_id(definition.namespace, definition.database, &held.topic)
            .await?
        {
            Catalog::new(transaction)
                .set_topic_owner(topic, None)
                .await?;
        }
        Ok(Outcome::Done)
    }

    /// Drain the transaction's feed captures into their topics, in the same
    /// commit as the writes they describe.
    ///
    /// Called by the session's commit paths — an explicit `COMMIT`, a lone
    /// statement's own transaction, and `VERIFY`'s rehearsal — after every
    /// write has landed and before the commit is assembled. A refusal anywhere
    /// refuses the whole commit, which is the atomicity the feature promises:
    /// a captured write's message lands with it or not at all.
    ///
    /// # Errors
    ///
    /// [`Error::FeedFailed`] carrying the refusal of a capture, an
    /// unauthorized append, or a missing topic — a captured write whose
    /// message cannot land refuses the write.
    pub(crate) async fn flush_feeds(&mut self, transaction: &mut Transaction<'_>) -> Result<()> {
        let captures = transaction.take_feed_captures();
        if captures.is_empty() {
            return Ok(());
        }
        let mut definitions: BTreeMap<
            tessari_types::TableId,
            Option<tessari_storage::TableDefinition>,
        > = BTreeMap::new();
        for (address, old) in captures {
            let definition = match definitions.get(&address.table) {
                Some(held) => held.clone(),
                None => {
                    let held = Catalog::new(transaction).table(address.table).await?;
                    definitions.insert(address.table, held.clone());
                    held
                }
            };
            let Some(definition) = definition else {
                continue;
            };
            if definition.feeds.is_empty() {
                continue;
            }
            let new = match transaction.get(&address).await? {
                Some(bytes) => Some(
                    tessari_encoding::decode_payload(&bytes)
                        .map_err(tessari_storage::Error::from)?,
                ),
                None => None,
            };
            let old = match old {
                Some(bytes) => Some(
                    tessari_encoding::decode_payload(&bytes)
                        .map_err(tessari_storage::Error::from)?,
                ),
                None => None,
            };
            let kind = match (&old, &new) {
                (None, Some(_)) => WriteKind::Create,
                (Some(_), Some(_)) => WriteKind::Update,
                (Some(_), None) => WriteKind::Delete,
                // A record that was not there and is not there was not written.
                (None, None) => continue,
            };
            for feed in &definition.feeds {
                if !feed.captures(kind) {
                    continue;
                }
                self.append_capture(transaction, &definition.name, &address, feed, &old, &new)
                    .await?;
            }
        }
        Ok(())
    }

    /// One captured write's envelope, appended to its feed's topic.
    async fn append_capture(
        &mut self,
        transaction: &mut Transaction<'_>,
        table: &str,
        address: &tessari_storage::RecordAddress,
        feed: &FeedDeclaration,
        old: &Option<Value>,
        new: &Option<Value>,
    ) -> Result<()> {
        let kind = match (old, new) {
            (None, Some(_)) => WriteKind::Create,
            (Some(_), Some(_)) => WriteKind::Update,
            (Some(_), None) => WriteKind::Delete,
            (None, None) => return Ok(()),
        };
        // The images are what the writer may read, the two halves an event
        // body's `$before`/`$after` apply (fire.rs): a read of the record,
        // authorized as the writer's own would be, and then their field
        // grant. A writer who may write the table and not read it feeds no
        // image at all — the topic is not a way to copy what a grant hides
        // somewhere the writer can read.
        let probe = StatementKind::Get {
            target: RecordTarget {
                table: TableRef {
                    database: None,
                    name: Name {
                        text: table.to_owned(),
                        span: Span::new(0, 0),
                    },
                    span: Span::new(0, 0),
                },
                id: Identity::Fixed(address.id.clone()),
                span: Span::new(0, 0),
            },
        };
        let open = self.identity.user().is_none();
        let readable = self
            .authorize_as_refreshed(self.store, &probe, open, Span::new(0, 0))
            .await
            .is_ok();
        let visible = self.visible_in(transaction, address.table).await?;
        let image = |value: &Option<Value>| match value {
            Some(value) if readable => Some(crate::redact::seen(value.clone(), &visible)),
            _ => None,
        };
        let before_image = image(old);
        let after_image = image(new);
        let bindings = Parameters::from([
            ("event".to_owned(), Value::from(kind.word())),
            (
                "before".to_owned(),
                before_image.clone().unwrap_or(Value::None),
            ),
            (
                "after".to_owned(),
                after_image.clone().unwrap_or(Value::None),
            ),
            ("id".to_owned(), crate::info::id_value(address.id.clone())),
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
        let topic = Catalog::new(transaction)
            .table_id(address.namespace, address.database, &feed.topic)
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
        let mut envelope = BTreeMap::from([
            ("key".to_owned(), crate::info::id_value(address.id.clone())),
            ("op".to_owned(), Value::from(kind.word())),
            ("at".to_owned(), Value::Datetime(self.now_datetime())),
        ]);
        // A partitioned topic's envelope carries the field it routes by
        //: metadata, not image — present even in IMAGE KEYS
        // mode, from the images the writer may read. A field a grant hides
        // is absent, and the message routes by turn like any its key cannot
        // place.
        if let Some(TableKind::Topic(tessari_storage::TopicDeclaration {
            partitions: Some(partitioning),
            ..
        })) = topic_definition.as_ref().map(|held| &held.kind)
            && let Some(field) = &partitioning.by
            && !envelope.contains_key(field)
        {
            let held = [after_image.clone(), before_image.clone()]
                .into_iter()
                .flatten()
                .find_map(|image| match image {
                    Value::Object(fields) => fields.get(field).cloned(),
                    _ => None,
                });
            if let Some(held) = held {
                envelope.insert(field.clone(), held);
            }
        }
        if feed.image == FeedImage::Full {
            if let Some(before) = before_image {
                envelope.insert("before".to_owned(), before);
            }
            if let Some(after) = after_image {
                envelope.insert("after".to_owned(), after);
            }
        }
        let payload = Value::Object(envelope);

        // The append is authorized exactly as the writer's own CREATE on the
        // topic would be: a writer who may write the table but not append the
        // topic has the write refused, carrying the refusal.
        let topic_ref = TableRef {
            database: None,
            name: Name {
                text: feed.topic.clone(),
                span: Span::new(0, 0),
            },
            span: Span::new(0, 0),
        };
        let probe = StatementKind::Create {
            target: CreateTarget::Generated(topic_ref),
            value: Expr {
                kind: ExprKind::Literal(Value::None),
                span: Span::new(0, 0),
            },
            answer: Answer::Nothing,
            expire: None,
        };
        let open = self.identity.user().is_none();
        self.authorize_as_refreshed(self.store, &probe, open, Span::new(0, 0))
            .await
            .map_err(|cause| Error::FeedFailed {
                feed: feed.name.clone(),
                table: table.to_owned(),
                cause: Box::new(cause),
            })?;

        let context = Context {
            namespace: address.namespace,
            database: address.database,
        };
        let identity = self
            .free_identity(transaction, &context, topic, &payload, Span::new(0, 0))
            .await
            .map_err(|cause| Error::FeedFailed {
                feed: feed.name.clone(),
                table: table.to_owned(),
                cause: Box::new(cause),
            })?;
        // A split topic's message lands in the partition its key chooses
        //: the envelope carries the partition field when the
        // record does, and the key decides otherwise.
        let home = self
            .partition_for(topic_definition.as_ref(), &payload, Span::new(0, 0))?
            .unwrap_or(topic);
        let at = tessari_storage::RecordAddress::new(
            address.namespace,
            address.database,
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

    /// The engine's instant as a datetime value, for an envelope's `at`.
    pub(crate) fn now_datetime(&self) -> Datetime {
        let since = self
            .now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or((0_i64, 0_u32), |held| {
                (
                    i64::try_from(held.as_secs()).unwrap_or(i64::MAX),
                    held.subsec_nanos(),
                )
            });
        Datetime::new(since.0, since.1).unwrap_or_else(|| Datetime::from_seconds(since.0))
    }
}
