//! What a Kafka consumer quarantined, as `INFO FOR KAFKA CONSUMER` reports it
//! (Q-708).
//!
//! A parked payload is a message that was on its way into the destination
//! table, so it is shown to a caller who may read that table whole — the same
//! reach a `SELECT` of the table would have — and to nobody else. Asking about a
//! consumer demands only `manage`, which says nothing about reading what flows
//! through it; a caller without the read sees each entry with its payload
//! **withheld and said so**, never silently dropped, because a list that looked
//! shorter would read as a consumer that parked less.

use tessari_ql::{Identity, Name, RecordTarget, Span, StatementKind, TableRef};
use tessari_storage::{ConsumerDefinition, Transaction};
use tessari_types::{RecordId, Value};

use crate::error::Result;
use crate::session::Session;

impl Session<'_> {
    /// The messages `consumer` parked, payloads shown or withheld for this
    /// caller.
    pub(super) fn parked(
        &self,
        transaction: &mut Transaction<'_>,
        consumer: &ConsumerDefinition,
        destination: &str,
        span: Span,
    ) -> Result<Value> {
        let parked = transaction.quarantined(consumer.id)?;
        if self.reads_whole(transaction, consumer, destination, span)? {
            return Ok(Value::Array(parked));
        }
        let withheld = Value::from(
            format!("withheld — reading {destination} whole is not granted to this user").as_str(),
        );
        Ok(Value::Array(
            parked
                .into_iter()
                .map(|entry| match entry {
                    Value::Object(mut fields) => {
                        fields.insert("payload".to_owned(), withheld.clone());
                        Value::Object(fields)
                    }
                    other => other,
                })
                .collect(),
        ))
    }

    /// Whether this caller may read the consumer's destination whole: the read
    /// a `SELECT` of it would demand passes every check, from the database the
    /// consumer writes into, and no field grant narrows it.
    fn reads_whole(
        &self,
        transaction: &mut Transaction<'_>,
        consumer: &ConsumerDefinition,
        destination: &str,
        span: Span,
    ) -> Result<bool> {
        if self.identity.user().is_none() {
            // An anonymous caller reached here only on an open store.
            return Ok(true);
        }
        // The probe names the table by name, which resolves in the caller's
        // own database — so a caller asking from another one is not taken to
        // read a table of the same name there.
        let here = self.context(transaction, None, span)?;
        if here.namespace != consumer.namespace || here.database != consumer.database {
            return Ok(false);
        }
        let probe = StatementKind::Get {
            target: RecordTarget {
                table: TableRef {
                    database: None,
                    name: Name {
                        text: destination.to_owned(),
                        span,
                    },
                    span,
                },
                id: Identity::Fixed(RecordId::Int(0)),
                span,
            },
        };
        let reads = self.identity.allows(&probe, false, span).is_ok()
            && self.within_tenancy(&probe, span).is_ok()
            && self.within_authority(self.store, &probe, span).is_ok()
            && self.within_grants(self.store, &probe, span).is_ok();
        Ok(reads
            && self
                .visible_in(transaction, consumer.destination)?
                .is_none())
    }
}
