//! `INFO FOR TOPIC`: a topic's positions and how far each reader has read.

use std::collections::BTreeMap;

use tessari_ql::{Span, TableRef};
use tessari_storage::{Catalog, TableKind, Transaction};
use tessari_types::{Number, Value};

use crate::error::{Error, Result};
use crate::session::Session;

fn whole(count: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

impl Session<'_> {
    /// The report `INFO FOR TOPIC` answers.
    ///
    /// `last` is the last position the topic has given and `first` the first it
    /// still holds, so `last - first + 1` is what is kept. Each reader's `lag`
    /// is how many positions it has not yet been given — the number that says a
    /// reader is falling behind before retention says it has lost something.
    pub(crate) fn info_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let (context, table) = self.resolve_table(transaction, topic)?;
        let declared = match Catalog::new(transaction)
            .table(table)?
            .map(|found| found.kind)
        {
            Some(TableKind::Topic(declared)) => declared,
            _ => {
                return Err(Error::NotATopic {
                    table: topic.name.text.clone(),
                    span,
                });
            }
        };
        let held =
            transaction.topic_after(context.namespace, context.database, table, u64::MAX, 0)?;
        let last = held.last.unwrap_or(0);
        let readers = Catalog::new(transaction)
            .topic_positions(context.namespace, context.database, table)?
            .into_iter()
            .map(|(name, position)| {
                (
                    name,
                    Value::Object(BTreeMap::from([
                        ("position".to_owned(), whole(position)),
                        ("lag".to_owned(), whole(last.saturating_sub(position))),
                    ])),
                )
            })
            .collect();
        let mut report = BTreeMap::from([
            ("name".to_owned(), Value::from(topic.name.text.as_str())),
            ("last".to_owned(), whole(last)),
            ("consumers".to_owned(), Value::Object(readers)),
        ]);
        report.insert("first".to_owned(), held.first.map_or(Value::None, whole));
        if let Some(retain) = declared.retain {
            report.insert("retain".to_owned(), Value::Duration(retain));
        }
        if let Some(max) = declared.max_bytes {
            report.insert("max_bytes".to_owned(), whole(max));
        }
        Ok(report)
    }
}
