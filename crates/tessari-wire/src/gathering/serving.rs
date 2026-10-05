use super::*;

/// Answer `asked` for the peer the handshake proved to be `asker`, from `store`.
///
/// # Errors
///
/// [`Error::NotGathered`] with the reason when the peer may not have the shard
/// or this node cannot give it, and [`Error::Refused`] when the store cannot be
/// read.
pub(crate) fn serve(
    store: &Store,
    granted: &dyn Subscriptions,
    asker: [u8; NODE_ID_LEN],
    asked: &Gather,
    (budget, fold_records): (usize, usize),
) -> Result<Page> {
    let refused = |why: tessari_storage::Error| Error::Refused {
        message: why.to_string(),
        class: None,
    };
    let mut transaction = store.begin().map_err(refused)?;
    let definition = Catalog::new(&mut transaction)
        .table(asked.table)
        .map_err(refused)?
        .filter(|found| found.namespace == asked.namespace && found.database == asked.database);
    let Some(map) = definition.and_then(|found| found.shards) else {
        return Err(Error::NotGathered(Ungathered::NoSuchTable));
    };
    // The table is here and split, so a shard missing from its live spans is a
    // map that moved on one side or the other — retired here, or minted by a
    // change this node has not applied yet (ADR-0095 D4).
    let Some(span) = map.spans().find(|span| span.id == asked.shard) else {
        return Err(Error::NotGathered(Ungathered::MapMoved));
    };
    let shard_of = |shard| Reach::Shard(asked.namespace, asked.database, asked.table, shard);
    let entitled = granted.granted(asker)?.is_some_and(|over| {
        over.contains(Reach::Database(asked.namespace, asked.database))
            || map.spans().any(|each| over.contains(shard_of(each.id)))
    });
    if !entitled {
        return Err(Error::NotGathered(Ungathered::NotEntitled));
    }
    if store
        .served()
        .is_some_and(|over| !over.contains(shard_of(asked.shard)))
    {
        return Err(Error::NotGathered(Ungathered::NotHeld));
    }
    // Clamped to the shard's own span, so a window reaching past it is answered
    // with this shard's records and never a neighbour's this node may not hold.
    let from = match (span.from, asked.from.as_ref()) {
        (Some(start), Some(wanted)) => Some(if wanted > start { wanted } else { start }),
        (start, wanted) => wanted.or(start),
    };
    let to = match (span.to, asked.to.as_ref()) {
        (Some(end), Some((wanted, inclusive))) => Some(if wanted < end {
            (wanted, *inclusive)
        } else {
            (end, false)
        }),
        (Some(end), None) => Some((end, false)),
        (None, Some((wanted, inclusive))) => Some((wanted, *inclusive)),
        (None, None) => None,
    };
    if let Some(reduce) = &asked.reduce {
        let found = transaction
            .records_between(
                asked.namespace,
                asked.database,
                asked.table,
                Window { from, to },
                asked.after.as_ref(),
                fold_records,
            )
            .map_err(refused)?;
        transaction.rollback();
        let more = found.len() == fold_records;
        return folded(store, reduce, (found, more), budget);
    }
    if let Some(counting) = &asked.counting {
        let page = counted::counted_page(
            &mut transaction,
            asked,
            counting,
            Window { from, to },
            fold_records,
        );
        transaction.rollback();
        return page;
    }
    if let Some(ordered) = &asked.ordered {
        let page = ordered::ranked_page(
            store,
            &mut transaction,
            asked,
            ordered,
            Window { from, to },
            budget,
        );
        transaction.rollback();
        return page;
    }
    let found = transaction
        .records_between(
            asked.namespace,
            asked.database,
            asked.table,
            Window { from, to },
            asked.after.as_ref(),
            GATHER_PAGE_RECORDS,
        )
        .map_err(refused)?;
    transaction.rollback();
    let more = found.len() == GATHER_PAGE_RECORDS;
    // ADR-0097: narrowed after the page is read and before it is budgeted, so
    // what the condition drops costs nothing to send. The page then resumes
    // after the last record READ, which may be one nobody kept.
    let read_to = found.last().map(|(id, _)| id.clone());
    let narrowed = asked.pushed.is_some();
    let found = match &asked.pushed {
        Some(pushed) => {
            tessari_session::keeping(store, pushed, found).map_err(|why| Error::Refused {
                message: why.to_string(),
                class: None,
            })?
        }
        None => found,
    };
    // Enough is a promise about the asker's need, not a budget: the records past
    // it are not sent, and nothing follows them.
    let enough = asked
        .enough
        .map(|enough| usize::try_from(enough).unwrap_or(usize::MAX));
    let (found, more) = match enough {
        Some(enough) if found.len() >= enough => {
            let mut found = found;
            found.truncate(enough);
            (found, false)
        }
        _ => (found, more),
    };
    let mut more = more;
    let mut cut = false;
    let mut records = Vec::with_capacity(found.len());
    let mut bytes = 0_usize;
    for (id, record) in found {
        // At least one record per page, whatever its size, or a record larger
        // than the budget could never be fetched at all.
        if !records.is_empty() && bytes.saturating_add(record.len()) > budget {
            more = true;
            cut = true;
            break;
        }
        bytes = bytes.saturating_add(record.len());
        records.push((id, record));
    }
    // Cut by the budget, the next page begins after the last record sent, as it
    // always has; narrowed and not cut, after the last record read.
    let resume = if narrowed && more && !cut {
        read_to
    } else {
        None
    };
    Ok(Page {
        records,
        more,
        resume,
        reduced: None,
        counted: None,
    })
}
