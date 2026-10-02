//! `ANALYZE TABLE`: the planner's statistics taken, and what was taken.

use std::collections::BTreeMap;

use tessari_ql::TableRef;
use tessari_storage::Transaction;
use tessari_types::{Number, Value};

use crate::error::Result;
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// Take the statistics of every value index on `table`, on this node.
    ///
    /// The answer is one object per index summarised: how many entries it
    /// held, how many distinct values each leading run of its fields held, how
    /// many common values were kept and how many buckets its first field was
    /// divided into. The values themselves are not answered — they are stored
    /// as the index encodes them, and a summary that printed them would be a
    /// way to read a field through its index.
    pub(super) fn analyze_table(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        let taken = transaction.analyze_table(context.namespace, context.database, id)?;
        let count =
            |held: u64| Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)));
        let mut answer = Vec::with_capacity(taken.len());
        for (index, statistics) in taken {
            let mut summary = BTreeMap::new();
            summary.insert("index".to_owned(), Value::from(index.name.as_str()));
            summary.insert("entries".to_owned(), count(statistics.entries));
            summary.insert(
                "distinct".to_owned(),
                Value::Array(statistics.distinct.iter().copied().map(count).collect()),
            );
            summary.insert(
                "common".to_owned(),
                count(u64::try_from(statistics.common.len()).unwrap_or(u64::MAX)),
            );
            summary.insert(
                "buckets".to_owned(),
                count(u64::try_from(statistics.bounds.len().saturating_sub(1)).unwrap_or(u64::MAX)),
            );
            answer.push(Value::Object(summary));
        }
        Ok(Outcome::Value(Value::Array(answer)))
    }
}
