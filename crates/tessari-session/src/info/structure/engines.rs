use super::*;

impl Session<'_> {
    /// The tables in the selected database, narrowed to those this session may
    /// read.
    /// One graph, and the tables that belong to it.
    ///
    /// An empty list is a legitimate answer and is what a graph declared a
    /// moment ago reports: the graph exists, and reporting it as absent would
    /// make the first thing anyone does after declaring one look like a failure.
    /// A graph that does not exist is the different case, and refuses.
    ///
    /// Membership is read by filtering the database's tables rather than from a
    /// list held on the graph, because the membership already lives on the table
    /// — a second copy on the graph would be a fact able to disagree with
    /// itself, which is the same reason a bucket's chunk table is derived from
    /// its name rather than stored beside it.
    pub(in crate::info) fn info_graph(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .graph_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "graph",
                name: name.text.clone(),
                span,
            })?;
        let readable = self.readable_in(transaction)?;
        let mut names = Vec::new();
        for table in Catalog::new(transaction).tables_in(context.namespace, context.database)? {
            if table.graph != Some(id) || !nameable(&table.name) {
                continue;
            }
            if readable
                .as_ref()
                .is_some_and(|granted| !granted.contains(&table.id))
            {
                continue;
            }
            names.push(table.name);
        }
        // The edge kinds are listed beside the node tables rather than folded in
        // with them, because they are not tables: nothing can select from one,
        // and a report that mixed the two would invite a caller to try.
        let kinds: Vec<_> = Catalog::new(transaction)
            .edge_kinds_in(context.namespace, context.database)?
            .into_iter()
            .filter(|kind| kind.graph == id)
            .map(|kind| kind.name)
            .collect();
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("tables".to_owned(), by_name(names)),
            ("edges".to_owned(), by_name(kinds)),
        ]))
    }

    /// `INFO FOR VECTOR embeddings` — its width, its distance, and its recall.
    ///
    /// The third field is the reason this is not `INFO FOR TABLE`. A vector
    /// index answers approximately, so the only number that says whether its
    /// answers are worth having is the recall it was **measured** at — and that
    /// is a property of a measurement rather than of a declaration, which is why
    /// no table report could ever carry it.
    ///
    /// It reads `none` until the index is **built**, which is where the whole
    /// graph and every stored vector are in hand at once and the measurement is
    /// therefore free of extra reads. A store declared and filled but never
    /// rebuilt reports `none`, and that is the honest answer rather than a gap:
    /// a figure derived from the build parameters would be a number nobody
    /// checked, wearing the name of one somebody did.
    ///
    /// When there is a figure it never appears alone. It carries the `k` it was
    /// measured at, how many queries it averaged, **how many records the store
    /// held at the time**, and the engine constants in force — because recall
    /// decays as records are added after a build, so a bare percentage goes
    /// stale in silence, which is the same failure by the other door.
    pub(in crate::info) fn info_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "vector store",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        let TableKind::Vector(declared) = definition.kind else {
            return Err(missing());
        };
        // The store's index, found by the property that makes it one rather than
        // by the name the desugaring gave it — a name is a spelling and this is
        // the thing itself.
        let index = Catalog::new(transaction)
            .field_indexes_on(id)?
            .into_iter()
            .find(|index| index.vector.is_some());
        let (measured, footprint, quantized) = match index {
            Some(index) => (
                transaction.vector_recall(&index)?,
                transaction.vector_node_bytes(&index)?,
                index.quantized,
            ),
            None => (None, None, false),
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            (
                "dimension".to_owned(),
                Value::Number(Number::Integer(i64::from(declared.dimension))),
            ),
            ("distance".to_owned(), Value::from(declared.distance.name())),
            // `None` and not a zero. A recall of zero is a measurement saying
            // the index finds nothing; absence says nobody has asked. Reporting
            // the second as the first is the failure this field exists to avoid.
            ("recall".to_owned(), measured.map_or(Value::None, reported)),
            ("quantized".to_owned(), Value::Bool(quantized)),
            // Measured off the stored nodes: the average bytes one takes, beside
            // how many there are. `None` while the store holds no vector.
            (
                "node_bytes".to_owned(),
                footprint.map_or(Value::None, |(_, average, _)| {
                    Value::Number(Number::Integer(i64::try_from(average).unwrap_or(i64::MAX)))
                }),
            ),
            (
                "vector_bytes".to_owned(),
                footprint.map_or(Value::None, |(_, _, per_vector)| {
                    Value::Number(Number::Integer(
                        i64::try_from(per_vector).unwrap_or(i64::MAX),
                    ))
                }),
            ),
            (
                "nodes".to_owned(),
                footprint.map_or(Value::None, |(nodes, _, _)| {
                    Value::Number(Number::Integer(i64::try_from(nodes).unwrap_or(i64::MAX)))
                }),
            ),
        ]))
    }

    pub(in crate::info) fn info_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let missing = || Error::Unknown {
            entity: "geo store",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(missing)?;
        let definition = Catalog::new(transaction).table(id)?.ok_or_else(missing)?;
        if definition.kind != TableKind::Geo {
            return Err(missing());
        }
        // Found by the property that makes it the store's index rather than by
        // the name the desugaring gave it, for the reason the vector store gives:
        // a name is a spelling, and this is the thing itself.
        let index = Catalog::new(transaction)
            .field_indexes_on(id)?
            .into_iter()
            .find(|index| index.spatial);
        let measured = match &index {
            Some(index) => transaction.spatial_refinement(index)?,
            None => None,
        };
        Ok(BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("field".to_owned(), Value::from(GEO_FIELD)),
            (
                "index".to_owned(),
                index.map_or(Value::None, |index| Value::from(index.name.as_str())),
            ),
            // `None` and not a zero, for the reason the vector store's recall is:
            // a ratio of zero would read as a filter that admits nothing, where
            // absence says nobody measured. Absence also covers a store whose
            // records never reach one another, which has no refinement cost to
            // report rather than a perfect one.
            (
                "refinement".to_owned(),
                measured.map_or(Value::None, refining),
            ),
        ]))
    }
}
