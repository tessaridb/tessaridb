//! A feed's position in several logs, as the text a subscriber holds (Q-791).
//!
//! # Opaque on purpose
//!
//! A subscriber stores the cursor the last change it handled carried and sends
//! it back to resume. It never reads one: the entries name logs by this store's
//! own identifiers, which mean nothing anywhere else. The spelling is
//! `<namespace>.<database>:` and then `d=<n>` for the database's log and
//! `<table>.<shard>=<n>` for a shard's, comma-joined, each position the next to
//! read.
//!
//! # Why it names its database
//!
//! Positions in one database's logs are positions in no other's, and a cursor
//! carried to a feed over another database would otherwise be read as its own
//! and resume that feed from the wrong place with nothing in an error state.

use std::collections::BTreeMap;

use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, ShardId, TableId};

/// The cursor `positions` in `namespace`/`database` spell, one entry per home.
pub(super) fn spell(
    namespace: NamespaceId,
    database: DatabaseId,
    positions: &BTreeMap<Reach, Sequence>,
) -> String {
    let mut out = format!("{namespace}.{database}:");
    for (home, at) in positions {
        let name = match home {
            Reach::Shard(_, _, table, shard) => format!("{table}.{shard}"),
            _ => "d".to_owned(),
        };
        if !out.ends_with(':') {
            out.push(',');
        }
        out.push_str(&name);
        out.push('=');
        out.push_str(&at.get().to_string());
    }
    out
}

/// Read a cursor back, as positions per home in `namespace`/`database`.
///
/// # Errors
///
/// Names what is wrong with the text: a cursor this build cannot read is one it
/// would resume from the wrong place, so it is refused rather than guessed.
pub(super) fn read(
    text: &str,
    namespace: NamespaceId,
    database: DatabaseId,
) -> Result<BTreeMap<Reach, Sequence>, String> {
    let wrong = || format!("{text:?} is not a cursor a change of this feed carried");
    let (given_in, entries) = text.split_once(':').ok_or_else(wrong)?;
    if given_in != format!("{namespace}.{database}") {
        return Err(format!(
            "{text:?} was given by a feed over another database, and its positions mean \
             nothing in this one"
        ));
    }
    let mut positions = BTreeMap::new();
    for entry in entries.split(',') {
        let (name, at) = entry.split_once('=').ok_or_else(wrong)?;
        let at = Sequence::new(at.parse().map_err(|_| wrong())?);
        let home = if name == "d" {
            Reach::Database(namespace, database)
        } else {
            let (table, shard) = name.split_once('.').ok_or_else(wrong)?;
            Reach::Shard(
                namespace,
                database,
                TableId::new(table.parse().map_err(|_| wrong())?),
                ShardId::new(shard.parse().map_err(|_| wrong())?),
            )
        };
        if positions.insert(home, at).is_some() {
            return Err(wrong());
        }
    }
    Ok(positions)
}

#[cfg(test)]
mod tests;
