//! `TRIM` — removing a topic's oldest messages (W8c).

use tessari_ql::{Expr, Span, TableRef, TrimBound};
use tessari_storage::{TopicTrim, Transaction};
use tessari_types::{Number, Value};

use crate::error::Result;
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// `TRIM <topic> KEEP n | BEFORE p` — the answer is how many messages it
    /// removed. A reader whose next position went is told so by its next read,
    /// as one behind retention is.
    pub(crate) async fn trim_topic(
        &self,
        transaction: &mut Transaction<'_>,
        (topic, partition): (&TableRef, Option<&Expr>),
        bound: &TrimBound,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span).await?;
        let table = self
            .named_partition(transaction, topic, table, partition, span)
            .await?;
        let reason = "a whole number at or above zero";
        let trim = match bound {
            TrimBound::Keep(keep) => TopicTrim::Keep(self.whole(transaction, keep, reason).await?),
            TrimBound::Before(below) => {
                TopicTrim::Before(self.whole(transaction, below, reason).await?)
            }
        };
        let removed = transaction
            .trim_topic((context.namespace, context.database, table), trim)
            .await?;
        Ok(Outcome::Value(Value::Number(Number::Integer(
            i64::try_from(removed).unwrap_or(i64::MAX),
        ))))
    }
}
