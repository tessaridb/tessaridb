//! `INFO FOR TOPIC CONSUMER`: a topic consumer's declaration, what this node is
//! doing with it, and what it guarantees (ADR-0087).

use std::collections::BTreeMap;

use tessari_ql::{Name, Span};
use tessari_storage::{Catalog, Feed, Transaction};
use tessari_types::Value;

use super::consumers::described_common;
use super::running_state;
use crate::error::{Error, Result};
use crate::session::Session;

/// What a topic consumer promises, stated where it is configured.
fn topic_guarantees() -> Value {
    Value::Object(BTreeMap::from([
        (
            "delivery".to_owned(),
            Value::from("exactly once into this store"),
        ),
        (
            "how".to_owned(),
            Value::from(
                "the group read, the record writes and the acknowledgement commit in one \
                 transaction, so a refused or lost commit leaves the message unread and \
                 nothing written",
            ),
        ),
        (
            "idempotence".to_owned(),
            Value::from(
                "the identity field still decides the record: two messages with one identity \
                 land as one record holding the later, and a replay from a moved group \
                 position converges",
            ),
        ),
        (
            "outside".to_owned(),
            Value::from("effects outside this store are not covered"),
        ),
        (
            "schema".to_owned(),
            Value::from("declared, never inferred: a message field nobody mapped does not land"),
        ),
    ]))
}

impl Session<'_> {
    /// The report `INFO FOR TOPIC CONSUMER` answers: `declared`, `running` (this
    /// node) and `guarantees`, as the Kafka form's does.
    pub(super) fn info_topic_consumer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
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
        let Feed::Topic { table } = consumer.feed else {
            return Err(Error::WrongConsumerKind {
                name: name.text.clone(),
                kind: "Kafka",
                instead: format!("INFO FOR KAFKA CONSUMER {}", name.text),
                span,
            });
        };
        let destination = self.named_table(transaction, &consumer)?;
        let topic = Catalog::new(transaction)
            .tables_in(consumer.namespace, consumer.database)?
            .into_iter()
            .find(|held| held.id == table)
            .map_or_else(|| "<dropped>".to_owned(), |held| held.name);
        let mut declared = match described_common(&consumer, &destination) {
            Value::Object(fields) => fields,
            _ => BTreeMap::new(),
        };
        declared.insert("topic".to_owned(), Value::from(topic.as_str()));
        Ok(BTreeMap::from([
            ("declared".to_owned(), Value::Object(declared)),
            (
                "running".to_owned(),
                running_state(self.store.running().progress(&consumer.name).as_ref()),
            ),
            ("guarantees".to_owned(), topic_guarantees()),
        ]))
    }
}
