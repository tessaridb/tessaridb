use super::*;

/// The map a table declaration asks for, or the refusal that says why not.
///
/// The rules live here, beside the map, rather than in the grammar: a parser is
/// the wrong place for an invariant about a stored table, because nothing stops
/// a later caller building a shape by hand.
///
/// # Errors
///
/// [`Error::SplitOnAKindThatIsNotRecords`] for any kind but a table or a space,
/// [`Error::SplitNeedsGeneratedUuid`] for a counter identity on a table, and
/// [`Error::SplitPointsOutOfOrder`] for points that do not ascend strictly.
pub(crate) fn declared_for(table: &str, shape: &TableShape) -> Result<Option<ShardMap>> {
    if shape.split.is_empty() {
        return Ok(None);
    }
    let kind = match &shape.kind {
        TableKind::Table | TableKind::Space(_) => None,
        TableKind::Collection => Some("a collection"),
        TableKind::Bucket(_) => Some("a bucket"),
        TableKind::Edge(_) => Some("an edge table"),
        TableKind::Vector(_) => Some("a vector store"),
        TableKind::Geo => Some("a geo store"),
        TableKind::Vault(_) => Some("a vault"),
        TableKind::Queue(_) => Some("a queue"),
        TableKind::View(_) => Some("a view"),
        TableKind::Series(_) => Some("a series"),
        TableKind::Topic(_) => Some("a topic"),
    };
    if let Some(kind) = kind {
        return Err(Error::SplitOnAKindThatIsNotRecords {
            table: table.to_owned(),
            kind,
        });
    }
    // A node table of a graph is walked from its neighbours, and a walk has no
    // span to confine it to one shard; splitting one would make every traversal
    // on a node holding part of it answer from the part (G031 S3.3).
    if shape.graph.is_some() {
        return Err(Error::SplitOnAKindThatIsNotRecords {
            table: table.to_owned(),
            kind: "a node table of a graph",
        });
    }
    // The uuid rule answers an argument about GENERATED identities: a shared
    // counter serializes every insert through one place. A space's keys are
    // named by the caller, never generated, so the argument does not apply —
    // the same absence of a check that ALTER-time splitting of a space lives
    // with, now stated once, at the declare.
    if !matches!(shape.kind, TableKind::Space(_)) && shape.identity != IdentityKind::Uuid {
        return Err(Error::SplitNeedsGeneratedUuid {
            table: table.to_owned(),
        });
    }
    ShardMap::declared(&shape.split).map_err(|refused| match refused {
        Unsplittable::OutOfOrder { position } => Error::SplitPointsOutOfOrder {
            table: table.to_owned(),
            position: position.saturating_add(1),
        },
        // A shard id is a u32; four billion split points is not a declaration
        // anybody writes, and it is refused under the ordering name because the
        // list, as written, does not describe a map.
        Unsplittable::TooMany => Error::SplitPointsOutOfOrder {
            table: table.to_owned(),
            position: shape.split.len(),
        },
    })
}
