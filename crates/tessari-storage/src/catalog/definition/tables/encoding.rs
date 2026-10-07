use super::*;

impl TableDefinition {
    /// The value written to the catalog.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::from([
            (FIELD_ID.to_owned(), number(self.id.get())),
            (FIELD_NAMESPACE.to_owned(), number(self.namespace.get())),
            (FIELD_DATABASE.to_owned(), number(self.database.get())),
            (FIELD_NAME.to_owned(), Value::from(self.name.as_str())),
            (FIELD_SCHEMAFULL.to_owned(), Value::Bool(self.schemafull)),
            // Four named flags on disk. The kind is how this build talks about a
            // table, not a change to how one is stored, so no catalog entry is
            // touched, no migration step is owed, and a build without the kind
            // reads everything this one writes.
            (
                FIELD_EDGE.to_owned(),
                Value::Bool(matches!(self.kind, TableKind::Edge(_))),
            ),
            (
                FIELD_BUCKET.to_owned(),
                Value::Bool(matches!(self.kind, TableKind::Bucket(_))),
            ),
            (
                FIELD_COLLECTION.to_owned(),
                Value::Bool(self.kind == TableKind::Collection),
            ),
            (
                FIELD_GEO.to_owned(),
                Value::Bool(self.kind == TableKind::Geo),
            ),
            (FIELD_IDENTITY.to_owned(), Value::from(self.identity.name())),
        ]);
        // Written only when there is one, for the reason the endpoint pair is:
        // a membership nobody declared is absent rather than zero, and zero is
        // a graph id the allocator can legitimately never hand out but which a
        // future reader would have to know that about.
        if let Some(graph) = self.graph {
            fields.insert(FIELD_GRAPH.to_owned(), number(graph.get()));
        }
        // Written only when the operator said something, for the reason the
        // graph membership above is: silence and a declared refusal are
        // different facts, and a policy word written for every table would make
        // them the same one.
        if let Some(conflict) = self.conflict {
            fields.insert(FIELD_CONFLICT.to_owned(), conflict.to_value());
        }
        // Written only when the table is split, for the same reason: an
        // unsharded table's entry stays byte-for-byte what it was.
        if let Some(shards) = &self.shards {
            fields.insert(FIELD_SHARDS.to_owned(), shards.to_value());
        }
        if let Some(partition) = &self.partition {
            fields.insert(FIELD_PARTITION.to_owned(), Value::from(partition.as_str()));
        }
        if self.spread {
            fields.insert(FIELD_SPREAD.to_owned(), Value::Bool(true));
        }
        if let Some(policy) = self.auto_split {
            fields.insert(FIELD_AUTO_SPLIT.to_owned(), policy.to_value());
        }
        if let Some(expire) = self.expire {
            fields.insert(FIELD_EXPIRE.to_owned(), expire.to_value());
        }
        if !self.events.is_empty() {
            fields.insert(
                FIELD_EVENTS.to_owned(),
                Value::Array(
                    self.events
                        .iter()
                        .map(super::super::EventDeclaration::to_value)
                        .collect(),
                ),
            );
        }
        // A declaration is not a flag, so it is written only by the edge table
        // that has one. Absent is how every edge table declared without a pair
        // reads, which is the same compatibility contract the flags keep: the
        // clause is optional, so an entry written before it existed decodes as
        // the permissive edge table it is.
        if let TableKind::Edge(Some(endpoints)) = &self.kind {
            fields.insert(FIELD_ENDPOINTS.to_owned(), endpoints.to_value());
        }
        // The vector store is the one kind carried by its declaration rather
        // than by a flag beside it, because it is the one kind with nothing to
        // say when the declaration is absent: an edge table without endpoints is
        // still an edge table, while a vector store without a width and a
        // distance is not a vector store at all. Presence is therefore the whole
        // statement, and a fourth flag would be a second place holding one fact.
        if let TableKind::Vector(declared) = &self.kind {
            fields.insert(FIELD_VECTOR.to_owned(), declared.to_value());
        }
        // A vault is carried by its declaration for the reason a vector store
        // is, and with more riding on it: presence is the whole statement,
        // because a vault without its key is not a vault with a missing
        // property — it is a store whose records are permanently unreadable.
        // The downgrade case, stated because it is easy to assume the opposite:
        // a build that predates vaults sets no flag, finds no vector, and reads
        // this entry as a plain **table**. It would then let `SELECT` return the
        // records. What it returns is ciphertext — the plaintext is not in the
        // store to be served — so the secrets hold, but every refusal the word
        // carries is gone. Opening a store with an older binary is therefore a
        // real downgrade and not merely a loss of the word (Q-412).
        if let TableKind::Vault(declared) = &self.kind {
            fields.insert(FIELD_VAULT.to_owned(), declared.to_value());
        }
        // Written only by the bucket that declared one, on the endpoint pair's
        // contract rather than the flags': a ceiling nobody declared is absent
        // rather than zero, and zero is the one value that would have to mean
        // "unbounded" while reading as "accepts nothing".
        if let TableKind::Bucket(Some(max)) = self.kind {
            fields.insert(FIELD_CEILING.to_owned(), byte_count(max));
        }
        // A queue is carried by its declaration for the reason a vector store
        // and a vault are: a queue with no timeout is not a queue with a missing
        // property, it is a table whose holds would never lapse. The downgrade
        // case is milder than the vault's and is still worth stating: a build
        // that predates queues finds no flag and no declaration and reads this
        // entry as a plain **table**, so the records are readable, the claim
        // fields are ordinary fields, and every refusal the word carries is
        // gone — the same shape of loss, without the confidentiality.
        if let TableKind::Queue(declared) = &self.kind {
            fields.insert(FIELD_QUEUE.to_owned(), declared.to_value());
        }
        // A view is carried by its read for the reason a queue is carried by its
        // timeout. Its downgrade case is the **sharpest of the three** and is
        // worth stating plainly: a build that predates views finds no flag and
        // no declaration and reads this entry as a plain table — one whose
        // keyspace is empty. So `SELECT` answers **nothing** rather than the
        // view's records, which is a wrong answer and not a lost refusal, and
        // `CREATE` succeeds and writes records into a prefix this build will
        // never read. Opening a store holding views with an older binary is a
        // downgrade with data consequences.
        if let TableKind::View(declared) = &self.kind {
            fields.insert(FIELD_VIEW.to_owned(), declared.to_value());
        }
        // A series is carried by its retention for the reason a queue is carried
        // by its timeout. Its downgrade case is the mildest of the four and is
        // still worth stating: a build that predates the kind finds no flag and
        // no declaration and reads this entry as a plain table, so every record
        // is readable — including the ones past the floor, which this build
        // hides. The loss is a refusal rather than an answer, the queue's shape
        // and not the view's.
        if let TableKind::Series(declared) = &self.kind {
            fields.insert(FIELD_SERIES.to_owned(), declared.to_value());
        }
        // A space is carried by its declaration for the reason a series is. A
        // build that predates the kind reads a plain schemaless table, which is
        // what a space was until G036, and loses only the limit.
        if let TableKind::Space(declared) = &self.kind {
            fields.insert(FIELD_SPACE.to_owned(), declared.to_value());
        }
        // A topic the same way (G037). A build that predates the kind reads a
        // plain schemaless table and would let a message be rewritten.
        if let TableKind::Topic(declared) = &self.kind {
            fields.insert(FIELD_TOPIC.to_owned(), declared.to_value());
        }
        Value::Object(fields)
    }

    /// Read a definition back.
    ///
    /// An entry written before a flag existed does not carry it, and reads as
    /// `false` — which is what such a table is. Refusing it instead would make an
    /// added property unreadable rather than absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field is missing or holds the
    /// wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let fields = object(value, "table")?;
        Ok(Self {
            id: TableId::new(field_id(fields, FIELD_ID, "table")?),
            namespace: NamespaceId::new(field_id(fields, FIELD_NAMESPACE, "table")?),
            database: DatabaseId::new(field_id(fields, FIELD_DATABASE, "table")?),
            name: field_name(fields, "table")?,
            schemafull: flag(fields, FIELD_SCHEMAFULL, "table")?,
            kind: TableKind::from_parts(StoredKind {
                edge: flag(fields, FIELD_EDGE, "table")?,
                bucket: flag(fields, FIELD_BUCKET, "table")?,
                collection: flag(fields, FIELD_COLLECTION, "table")?,
                geo: flag(fields, FIELD_GEO, "table")?,
                endpoints: match fields.get(FIELD_ENDPOINTS) {
                    Some(value) => Some(EdgeDeclaration::from_value(value)?),
                    None => None,
                },
                vector: match fields.get(FIELD_VECTOR) {
                    Some(value) => Some(VectorDeclaration::from_value(value)?),
                    None => None,
                },
                vault: match fields.get(FIELD_VAULT) {
                    Some(value) => Some(VaultDeclaration::from_value(value)?),
                    None => None,
                },
                queue: match fields.get(FIELD_QUEUE) {
                    Some(value) => Some(QueueDeclaration::from_value(value)?),
                    None => None,
                },
                view: match fields.get(FIELD_VIEW) {
                    Some(value) => Some(ViewDeclaration::from_value(value)?),
                    None => None,
                },
                series: match fields.get(FIELD_SERIES) {
                    Some(value) => Some(SeriesDeclaration::from_value(value)?),
                    None => None,
                },
                space: match fields.get(FIELD_SPACE) {
                    Some(value) => {
                        Some(crate::catalog::space::SpaceDeclaration::from_value(value)?)
                    }
                    None => None,
                },
                topic: match fields.get(FIELD_TOPIC) {
                    Some(value) => {
                        Some(crate::catalog::topic::TopicDeclaration::from_value(value)?)
                    }
                    None => None,
                },
                ceiling: ceiling(fields)?,
            })?,
            identity: identity_kind(fields, "table")?,
            graph: match fields.get(FIELD_GRAPH) {
                Some(_) => Some(GraphId::new(field_id(fields, FIELD_GRAPH, "table")?)),
                None => None,
            },
            conflict: match fields.get(FIELD_CONFLICT) {
                None => None,
                Some(held) => Some(ConflictPolicy::from_value(held).ok_or(
                    Error::CatalogMalformed {
                        entity: "table",
                        field: FIELD_CONFLICT,
                        found: "a conflict policy this build does not have",
                    },
                )?),
            },
            shards: match fields.get(FIELD_SHARDS) {
                None => None,
                Some(held) => Some(ShardMap::from_value(held)?),
            },
            partition: match fields.get(FIELD_PARTITION) {
                None => None,
                Some(Value::String(field)) => Some(field.clone()),
                Some(_) => {
                    return Err(Error::CatalogMalformed {
                        entity: "table",
                        field: FIELD_PARTITION,
                        found: "a partition field that is not a name",
                    });
                }
            },
            spread: match fields.get(FIELD_SPREAD) {
                None => false,
                Some(Value::Bool(spread)) => *spread,
                Some(_) => {
                    return Err(Error::CatalogMalformed {
                        entity: "table",
                        field: FIELD_SPREAD,
                        found: "a spread that is not true or false",
                    });
                }
            },
            auto_split: fields
                .get(FIELD_AUTO_SPLIT)
                .map(super::super::AutoSplit::from_value)
                .transpose()?,
            expire: fields
                .get(FIELD_EXPIRE)
                .map(super::super::TableExpiry::from_value)
                .transpose()?,
            events: match fields.get(FIELD_EVENTS) {
                None => Vec::new(),
                Some(Value::Array(held)) => held
                    .iter()
                    .map(super::super::EventDeclaration::from_value)
                    .collect::<Result<_>>()?,
                Some(other) => {
                    return Err(Error::CatalogMalformed {
                        entity: "table",
                        field: FIELD_EVENTS,
                        found: other.type_name(),
                    });
                }
            },
        })
    }
}
