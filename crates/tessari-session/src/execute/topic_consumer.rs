//! `DEFINE TOPIC CONSUMER` and `DROP TOPIC CONSUMER` (ADR-0087).

use tessari_ql::{FieldMapping, FieldPath, Name, Span, TableRef};
use tessari_storage::{Catalog, ConsumerDefinition, Feed, OnFailure, Transaction};

use super::cluster::{failure_policy, mapped};
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// A topic consumer as a statement declares it.
pub(super) struct TopicDeclared<'a> {
    pub(super) name: &'a Name,
    pub(super) topic: &'a TableRef,
    pub(super) group: &'a str,
    pub(super) identity: &'a FieldPath,
    pub(super) mapping: &'a [FieldMapping],
    pub(super) destination: &'a TableRef,
    pub(super) on_failure: tessari_ql::OnFailure,
    pub(super) parallelism: Option<u32>,
}

impl Session<'_> {
    /// Declare a topic consumer, after checking everything it names.
    ///
    /// Every refusal comes before the name is claimed, as for the Kafka form:
    /// the topic is a topic, the group is declared on it, the destination
    /// exists **in the same database** (one transaction is the guarantee), and a
    /// `quarantine` policy has a group that will eventually stop handing a
    /// message out.
    pub(super) fn define_topic_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        declared: &TopicDeclared<'_>,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        if if_not_exists
            && Catalog::new(transaction)
                .consumers()?
                .iter()
                .any(|found| found.name == declared.name.text)
        {
            return Ok(Outcome::Done);
        }

        let (context, topic) = self.group_topic(transaction, declared.topic, span)?;
        let group = Self::existing_group(
            transaction,
            &context,
            topic,
            declared.group,
            declared.topic,
            span,
        )?;
        let (landing, destination) = self.resolve_table(transaction, declared.destination)?;
        if landing.namespace != context.namespace || landing.database != context.database {
            return Err(Error::TopicConsumerSpansDatabases {
                topic: declared.topic.name.text.clone(),
                destination: declared.destination.name.text.clone(),
                span,
            });
        }
        let on_failure = failure_policy(declared.on_failure);
        if on_failure == OnFailure::Quarantine {
            let missing = match (group.declaration.deliveries, group.declaration.dead_letter) {
                (None, _) => Some("`DELIVERIES`"),
                (Some(_), None) => Some("`DEAD LETTER TO`"),
                (Some(_), Some(_)) => None,
            };
            if let Some(missing) = missing {
                return Err(Error::QuarantineNeedsDeadLetter {
                    group: declared.group.to_owned(),
                    missing,
                    span,
                });
            }
        }

        let definition = ConsumerDefinition {
            // Replaced by the catalog when the record is written.
            id: 0,
            name: declared.name.text.clone(),
            feed: Feed::Topic { table: topic },
            group: declared.group.to_owned(),
            identity: declared.identity.path.to_string(),
            mapping: mapped(declared.mapping)?,
            namespace: context.namespace,
            database: context.database,
            destination,
            on_failure,
            // `None` reads as one; the parser has already refused a zero.
            parallelism: declared.parallelism.unwrap_or(1),
            // Whose authority each batch carries, re-established every batch.
            declarer: self.identity.user().map(|user| user.id),
        };
        Catalog::new(transaction).create_consumer(&definition)?;
        Ok(Outcome::Done)
    }

    /// Forget a topic consumer; the runner stops its members after the batch
    /// each is in, the next time it reads the catalog.
    pub(super) fn drop_topic_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .consumers()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(consumer) = found else {
            return Err(Error::Unknown {
                entity: "topic consumer",
                name: name.text.clone(),
                span,
            });
        };
        if matches!(consumer.feed, Feed::Kafka { .. }) {
            return Err(Error::WrongConsumerKind {
                name: name.text.clone(),
                kind: "Kafka",
                instead: format!("DROP KAFKA CONSUMER {}", name.text),
                span,
            });
        }
        Catalog::new(transaction).drop_consumer(&consumer)?;
        Ok(Outcome::Done)
    }
}
