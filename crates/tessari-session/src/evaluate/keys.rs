//! Record keys and addresses: keys taken from values, a record addressed by its
//! table, a value read by its key, and the indexes that keep keys in order.

use std::collections::{BTreeMap, BTreeSet};

use tessari_ql::{RecordTarget, Select, Source, Span};
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{Number, RecordId, TableId, Value};

use crate::budget::Ceiling;
use crate::error::{Error, Result};
use crate::session::Session;

use super::Answered;

/// The record identity a range bound names.
///
/// A key is a record id, so a bound has to be one of the four kinds an identity
/// has; anything else is a bound that could never match a key.
pub(crate) fn key_bound(value: &Value, span: Span) -> Result<RecordId> {
    match value {
        Value::Number(Number::Integer(id)) => Ok(RecordId::Int(*id)),
        Value::String(text) => Ok(RecordId::Text(text.clone())),
        Value::Uuid(bytes) => Ok(RecordId::Uuid(*bytes)),
        Value::Bytes(bytes) => Ok(RecordId::Bytes(bytes.clone())),
        _ => Err(Error::InvalidKeyBound { span }),
    }
}

/// What a source produced: the records, how they were reached, and what its
/// searched fields need.
///
/// The searched context travels with the records because a sort key is an
/// expression too, and one holding a `MATCHES` or a score must mean the same
/// thing there as in the `WHERE` that produced them.
/// The ordered index that serves a join key, when there is one.
///
/// Ordered and single-field only. A search index holds terms rather than whole
/// values and a vector index answers a distance, so neither can answer "which
/// records hold exactly this"; a composite index answers a question about its
/// first field and this is not that question unless it is the only field.
/// File records into an ordered map under the value at one route.
///
/// A record with nothing at the route contributes nothing: `NONE` is a value
/// and the other side would have to carry it to match, which is what an inner
/// join means.
pub(crate) fn collect_by_key(
    into: &mut BTreeMap<Value, Vec<(RecordId, Value)>>,
    records: Vec<(RecordId, Value)>,
    key: &tessari_ql::FieldPath,
) {
    for (id, record) in records {
        let Some(found) = key.path.resolve(&record).cloned() else {
            continue;
        };
        into.entry(found).or_default().push((id, record));
    }
}

/// Kind names as a reader would say them: `record`, or `record and string`.
///
/// A join key usually holds one kind, so the common message reads as a bare
/// noun rather than as a set with one element in it.
pub(crate) fn listed(kinds: &BTreeSet<&'static str>) -> String {
    let held: Vec<&str> = kinds.iter().copied().collect();
    match held.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

pub(crate) fn ordered_index_on(
    transaction: &mut Transaction<'_>,
    table: TableId,
    key: &tessari_ql::FieldPath,
) -> Result<Option<tessari_storage::IndexDefinition>> {
    if !transaction.indexes_are_current_for(table)? {
        return Ok(None);
    }
    Ok(Catalog::new(transaction)
        .field_indexes_on(table)?
        .into_iter()
        .find(|held| {
            held.is_ordered() && held.fields.len() == 1 && held.fields.first() == Some(&key.path)
        }))
}

impl Session<'_> {
    /// Where a record lives, once its table name is resolved.
    pub(crate) fn address(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<(crate::context::Context, RecordAddress)> {
        let (context, table) = self.resolve_table(transaction, &target.table)?;
        Ok((
            context,
            RecordAddress::new(
                context.namespace,
                context.database,
                table,
                target.id.fixed(target.span)?.clone(),
            ),
        ))
    }

    /// The value under a key, or [`Value::None`] when there is nothing there.
    pub(crate) fn read_key(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<Value> {
        let (_, address) = self.address(transaction, target)?;
        let visible = self.visible_in(transaction, address.table)?;
        match transaction.get(&address)? {
            Some(payload) => self.record_of(&payload, &visible),
            None => Ok(Value::None),
        }
    }

    /// A read standing where a value stands.
    ///
    /// One record answers with its own value; a read of several answers with an
    /// array, so that the shape of the answer follows the shape of the question
    /// rather than the number of rows that happened to match.
    pub(super) fn read_as_value(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Value> {
        // The notes are dropped here, and this is the one place they are. An
        // expression position has no channel to carry them: the answer *is* a
        // value, and a value has no room beside it. Reported at the statement
        // that holds this one would be worse than silence — a note about an
        // inner read, attached to an outer answer it does not describe.
        // `None` for the deadline, for the same reason the notes are dropped: an
        // expression position has no channel to carry one *in* either, so a read
        // standing here enforces its own ceiling and not its caller's.
        //
        // The held ceiling is the one thing that does reach here, and this is the
        // position that most needs it: the answer is a `Value` built whole, so an
        // unbounded read is an unbounded array, and there is no note channel a
        // truncating default could have reported through.
        let Answered { records, .. } =
            self.read(transaction, select, None, Ceiling::over(select))?;
        // `$node` alongside `Source::Record` because it is one record too: a
        // read of one answers with its own value, and wrapping it in an array of
        // one would make the shape of the answer follow the source rather than
        // the question.
        //
        // `ONLY` says the same thing about a source that could have answered
        // with many — `FROM ONLY users WHERE email = $e` — which is the half of
        // this rule the source alone cannot tell. The read has already refused
        // if more than one answered, so there is at most one here either way.
        if select.only.is_some() || matches!(select.from, Source::Record(_) | Source::Node) {
            return Ok(records
                .into_iter()
                .next()
                .map_or(Value::None, |(_, value)| value));
        }
        Ok(Value::Array(
            records.into_iter().map(|(_, value)| value).collect(),
        ))
    }
}
