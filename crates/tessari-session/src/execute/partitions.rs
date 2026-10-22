//! `DEFINE TOPIC … PARTITIONS n [BY field]`: a parent holding no message and n
//! child topics, declared in one transaction (ADR-0131).

use tessari_ql::{Name, Span, TopicClauses};
use tessari_storage::{
    Catalog, Partitioning, TableDefinition, TableKind, TableShape, TopicDeclaration, Transaction,
};
use tessari_types::{Number, TableId, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// Declare the children `name#0 … name#(n-1)`, each a topic carrying the
    /// declaration's clauses, then the parent naming them in order.
    ///
    /// `#` is not an identifier character, so no statement can name a child and
    /// every listing that shows only nameable tables leaves them out.
    pub(super) async fn define_partitioned_topic(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        clauses: &TopicClauses,
        (count, by): (u64, Option<&str>),
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        if count > tessari_constants::TOPIC_MAX_PARTITIONS {
            return Err(Error::PartitionsAboveCeiling { asked: count, span });
        }
        self.refuse_a_format_the_store_does_not_hold(
            "a topic split into partitions",
            tessari_encoding::FormatVersion::SPLIT_TOPIC,
        )
        .await?;
        let context = self.context(transaction, None, span).await?;
        let (namespace, database) = (context.namespace, context.database);
        let declared = crate::topic::declared_topic(clauses);
        let shape = |declaration: TopicDeclaration| TableShape {
            kind: TableKind::Topic(declaration),
            ..TableShape::default()
        };
        let exists = Catalog::new(transaction)
            .table_id(namespace, database, &name.text)
            .await?
            .is_some();
        if exists {
            if if_not_exists {
                return Ok(Outcome::Done);
            }
            // The ordinary refusal for a name already taken, in its own words.
            Catalog::new(transaction)
                .create_table(namespace, database, &name.text, shape(declared))
                .await?;
            return Ok(Outcome::Done);
        }
        let mut children = Vec::new();
        for index in 0..count {
            let child = Catalog::new(transaction)
                .create_table(
                    namespace,
                    database,
                    &format!("{}#{index}", name.text),
                    shape(declared.clone()),
                )
                .await?;
            children.push(child.id);
        }
        let parent = TopicDeclaration {
            partitions: Some(Partitioning {
                by: by.map(str::to_owned),
                children,
            }),
            ..declared
        };
        Catalog::new(transaction)
            .create_table(namespace, database, &name.text, shape(parent))
            .await?;
        Ok(Outcome::Done)
    }
}

impl Session<'_> {
    /// The partition an append to `definition` lands in, or `None` when the
    /// table is not a split topic.
    ///
    /// By the key field's bytes, hashed as the Kafka clients' default
    /// partitioner hashes a record's key, so a key lands in one partition
    /// whichever side wrote it; a message without the key takes this node's
    /// next turn, as a record with a null key does.
    ///
    /// # Errors
    ///
    /// [`Error::PartitionKey`] when the key holds a value no partition can be
    /// chosen by.
    pub(crate) fn partition_for(
        &self,
        definition: Option<&TableDefinition>,
        payload: &Value,
        span: Span,
    ) -> Result<Option<TableId>> {
        let Some(TableDefinition {
            id,
            kind:
                TableKind::Topic(TopicDeclaration {
                    partitions: Some(partitioning),
                    ..
                }),
            ..
        }) = definition
        else {
            return Ok(None);
        };
        let count = u64::try_from(partitioning.children.len()).unwrap_or(u64::MAX);
        let held = match (&partitioning.by, payload) {
            (Some(field), Value::Object(fields)) => fields.get(field).map(|held| (field, held)),
            _ => None,
        };
        let turn = match held {
            None | Some((_, Value::None | Value::Null)) => self.store.partition_turn(*id),
            Some((field, held)) => {
                let key = match held {
                    Value::String(key) => key.as_bytes().to_vec(),
                    Value::Bytes(key) => key.to_vec(),
                    Value::Number(Number::Integer(key)) => key.to_string().into_bytes(),
                    Value::Uuid(key) => key.to_vec(),
                    other => {
                        return Err(Error::PartitionKey {
                            field: field.clone(),
                            found: other.type_name(),
                            span,
                        });
                    }
                };
                u64::from(murmur2(&key) & 0x7fff_ffff)
            }
        };
        let place = usize::try_from(turn.checked_rem(count).unwrap_or(0)).unwrap_or(0);
        Ok(partitioning.children.get(place).copied())
    }
}

impl Session<'_> {
    /// The table an append of `payload` to `table` writes: the partition it
    /// lands in when `table` is a split topic, otherwise `table` itself.
    ///
    /// For the writers that append a message they made themselves — a dead
    /// letter — rather than one a statement named.
    ///
    /// # Errors
    ///
    /// As [`Self::partition_for`], and a catalog failure.
    pub(crate) async fn appended_home(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        payload: &Value,
        span: Span,
    ) -> Result<TableId> {
        let definition = Catalog::new(transaction).table(table).await?;
        Ok(self
            .partition_for(definition.as_ref(), payload, span)?
            .unwrap_or(table))
    }
}

/// The 32-bit murmur2 hash the Kafka clients' default partitioner applies to a
/// record's key: seed `0x9747b28c`, four bytes at a time little-endian, the
/// tail folded in from its last byte down.
fn murmur2(data: &[u8]) -> u32 {
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let mut hash = 0x9747_b28c ^ length;
    let (chunks, tail) = data.as_chunks::<4>();
    for chunk in chunks {
        let mut k = u32::from_le_bytes(*chunk);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        hash = hash.wrapping_mul(M) ^ k;
    }
    let folded = match *tail {
        [first, second, third] => {
            Some(u32::from(third) << 16 | u32::from(second) << 8 | u32::from(first))
        }
        [first, second] => Some(u32::from(second) << 8 | u32::from(first)),
        [first] => Some(u32::from(first)),
        _ => None,
    };
    if let Some(folded) = folded {
        hash = (hash ^ folded).wrapping_mul(M);
    }
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(M);
    hash ^ (hash >> 15)
}

#[cfg(test)]
mod tests {
    use super::murmur2;

    /// The vectors the Kafka clients' own partitioner answers (kafka-python
    /// `murmur2`, unsigned; "21" and "foobar" are its published test vectors).
    #[test]
    fn the_hash_is_the_one_kafka_clients_choose_a_partition_by() {
        for (key, expected) in [
            ("", 275_646_681_u32),
            ("21", 3_321_034_988),
            ("abc", 479_470_107),
            ("foobar", 3_504_634_814),
            ("a-little-bit-long-string", 3_308_985_760),
            ("user-42", 1_459_644_460),
            ("ключ", 2_122_343_024),
        ] {
            assert_eq!(murmur2(key.as_bytes()), expected, "{key:?}");
        }
    }
}
