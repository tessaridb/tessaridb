//! The broker, as one implementation of [`Source`].
//!
//! Behind the `kafka` feature, because the client vendors and compiles
//! librdkafka and therefore needs a C toolchain, which an ordinary build of this
//! database must not (ADR-0024 §2).
//!
//! # The two settings that are not configurable
//!
//! **`enable.auto.commit = false`.** Automatic commits move the offset on a
//! timer, independently of whether the store commit succeeded. That inverts the
//! one ordering that is the whole delivery guarantee and turns at-least-once
//! into silent loss. It is set here rather than exposed, because a guarantee the
//! operator can switch off is not a guarantee.
//!
//! **`enable.partition.eof = false`.** Reaching the end of a partition is the
//! ordinary state of a live consumer, not an event, and surfacing it would make
//! the runner's `Ok(None)` — *nothing arrived* — indistinguishable from a
//! failure.
//!
//! Everything else is the client's own default, read from the client rather than
//! restated here.

use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer as _};
use rdkafka::message::Message as _;
use tessari_storage::ConsumerDefinition;

use crate::runner::Broker;
use crate::source::{Message, Source, SourceError};

/// Opens consumers against a real broker.
#[derive(Debug, Default, Clone, Copy)]
pub struct Kafka;

impl Broker for Kafka {
    fn open(&self, definition: &ConsumerDefinition) -> Result<Box<dyn Source>, SourceError> {
        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", definition.brokers.join(","))
            // Declared, never derived. Deriving it from the node id would be a
            // bug that appears only in a cluster, where every node would form
            // its own group and every node would consume every message.
            .set("group.id", &definition.group)
            .set("enable.auto.commit", "false")
            .set("enable.partition.eof", "false")
            .create()
            .map_err(|failure| SourceError(failure.to_string()))?;
        consumer
            .subscribe(&[definition.topic.as_str()])
            .map_err(|failure| SourceError(failure.to_string()))?;
        Ok(Box::new(Connected { consumer }))
    }
}

/// One consumer, subscribed.
struct Connected {
    consumer: BaseConsumer,
}

impl Source for Connected {
    fn poll(&mut self, patience: Duration) -> Result<Option<Message>, SourceError> {
        let Some(held) = self.consumer.poll(patience) else {
            return Ok(None);
        };
        let message = held.map_err(|failure| SourceError(failure.to_string()))?;
        Ok(Some(Message {
            partition: message.partition(),
            offset: message.offset(),
            // A message with no payload is a **tombstone** in this protocol, and
            // it reaches the runner as an empty payload rather than being
            // skipped here: what a consumer does with one is the runner's
            // decision to make and report, not this adapter's to take silently.
            payload: message.payload().unwrap_or_default().to_vec(),
        }))
    }

    fn commit(&mut self) -> Result<(), SourceError> {
        // Synchronous, so that returning means the broker has the position. An
        // asynchronous commit would return before the offset moved, and the
        // ordering this whole crate is built around would be a hope.
        self.consumer
            .commit_consumer_state(CommitMode::Sync)
            .map_err(|failure| SourceError(failure.to_string()))
    }
}
