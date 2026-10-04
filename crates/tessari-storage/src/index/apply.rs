use super::*;

pub(super) fn apply_one(
    store: &Store,
    mut batch: WriteBatch,
    definition: &IndexDefinition,
    mutation: &Mutation,
    previous: Option<&[u8]>,
    (analyzers, named): (&BTreeMap<String, Analyzer>, &BTreeMap<String, Analyzer>),
    pending: &mut Pending,
) -> Result<WriteBatch> {
    let address = IndexAddress::new(
        definition.namespace,
        definition.database,
        definition.table,
        definition.id,
    );

    if let Some(distance) = definition.vector {
        // The graph is read from committed state once per batch and edited,
        // then the nodes each edit touched are written; a node touched twice is
        // written twice and the later write, which holds both edits, lands last.
        // Reading the whole graph is the cost this shape pays, and it is stated
        // in `graph.rs` rather than discovered: an index over more vectors than
        // fit in memory wants a paging walk, which is not this.
        let graph =
            match pending.graphs.entry(address) {
                std::collections::btree_map::Entry::Occupied(held) => held.into_mut(),
                std::collections::btree_map::Entry::Vacant(empty) => empty.insert(
                    graph::Graph::read(store, &address, distance, definition.quantized)?,
                ),
            };
        let previous_vector = previous
            .map(decode_payload)
            .transpose()?
            .and_then(|held| projected_vector(definition, &held));
        if previous_vector.is_some() {
            graph.remove(&mutation.id);
            batch = graph::erase(batch, &address, &mutation.id);
        }
        if let RecordValue::Present(payload) = mutation.value.value()
            && let Some(held) = projected_vector(definition, &decode_payload(payload)?)
        {
            let touched = graph.insert(&mutation.id, held)?;
            batch = graph::write(batch, &address, &touched);
        }
        return Ok(batch);
    }

    if definition.spatial {
        // Both sides enumerate with the same function, so a record that kept its
        // geometry writes back exactly the keys it already had and a record that
        // changed it leaves none behind. Reasoning about *what moved* instead is
        // where an orphan cell would come from — and an orphan here is a record
        // answering a box it is no longer inside, which no reader would question
        // because the answer is geographically plausible.
        if let Some(bytes) = previous {
            batch = displace(
                batch,
                &address,
                &mutation.id,
                &decode_payload(bytes)?,
                definition,
            );
        }
        if let RecordValue::Present(payload) = mutation.value.value() {
            batch = place(
                batch,
                &address,
                &mutation.id,
                &decode_payload(payload)?,
                definition,
            );
        }
        return Ok(batch);
    }

    if definition.search || definition.engine.is_some() {
        let analyzer = analyzer_for(definition, analyzers, named);
        // An unscored index keeps no collection statistics (ADR-0100 D4): it
        // is never scored, so there is nothing for them to describe.
        let mut counted =
            (!definition.costs.unscored).then(|| pending.moved.entry(address).or_default());
        let dictionary = pending.terms.entry(address).or_default();
        // The old side first, and both sides of the same change: a record whose
        // text changed leaves the index at its former length and re-enters at
        // its new one, so a statistic that only counted arrivals would drift
        // upward by exactly the amount nobody ever looks at.
        //
        // The dictionary moves on the same two sides and by the same reasoning.
        // A word the record kept is decremented and incremented, netting zero,
        // so a rewrite that changed one sentence does not disturb the frequency
        // of every other word in the document.
        if let Some(bytes) = previous {
            let analysed = analysed(definition, analyzer, &decode_payload(bytes)?);
            if let Some(counted) = counted.as_mut() {
                counted.removed(analysed.tokens);
            }
            lengthen(&mut pending.lengths, address, &analysed.fields, false);
            surface(&mut pending.surfaces, address, analysed.surfaces, -1);
            for (term, _) in analysed.postings {
                dictionary.entry(term.clone()).or_default().left();
                batch = batch.delete(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, mutation.id.clone()).encode(),
                );
            }
        }
        if let RecordValue::Present(payload) = mutation.value.value() {
            let mut analysed = analysed(definition, analyzer, &decode_payload(payload)?);
            if let Some(counted) = counted.as_mut() {
                counted.added(analysed.tokens);
            }
            lengthen(&mut pending.lengths, address, &analysed.fields, true);
            surface(
                &mut pending.surfaces,
                address,
                std::mem::take(&mut analysed.surfaces),
                1,
            );
            let length = analysed.length();
            for ((term, frequency), located) in analysed.postings.into_iter().zip(&analysed.located)
            {
                dictionary
                    .entry(term.clone())
                    .or_default()
                    .arrived(frequency, length);
                batch = batch.put(
                    PostingKey::keyspace(),
                    PostingKey::new(address, term, mutation.id.clone()).encode(),
                    posted(definition, frequency, length, located),
                );
            }
        }
        return Ok(batch);
    }

    // The old entries go first: a record whose indexed value changed must not
    // leave the entry that pointed at its former value behind, and an entry
    // nothing will ever reconcile is the failure mode secondary indexes are
    // known for.
    //
    // With a multi-valued route there is one entry per element, so removing an
    // element has to remove **exactly its own** entry and no other. That holds
    // because both sides enumerate with the same function: the old record's
    // entries are deleted and the new record's are written, and the elements the
    // record kept are written back under the keys they already had. A remove that
    // reasoned about *what changed* instead is where the orphan would come from.
    if let Some(bytes) = previous {
        for values in project(definition, &decode_payload(bytes)?) {
            batch = remove(batch, definition, &address, &values, &mutation.id);
        }
    }

    if let RecordValue::Present(payload) = mutation.value.value() {
        for values in project(definition, &decode_payload(payload)?) {
            batch = insert(
                store,
                batch,
                definition,
                &address,
                &values,
                &mutation.id,
                &mut pending.claimed,
            )?;
        }
    }
    Ok(batch)
}
