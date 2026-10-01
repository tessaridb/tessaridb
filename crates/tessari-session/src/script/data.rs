//! One table's declaration and records, as the statements that make them again.

use std::fmt::Write as _;

use tessari_storage::{Catalog, GEO_FIELD, TableDefinition, TableKind, VECTOR_FIELD};
use tessari_types::Value;

use crate::error::Result;
use crate::outcome::Outcome;
use crate::session::Session;

/// How many records one transaction of the script carries.
const BATCH: usize = 1_000;

/// A table's declaration without its indexes, and its indexes apart — or the
/// part of it with no faithful spelling.
///
/// The indexes go after the data, as a dump's post-data section does: an index
/// declared over rows that are already there is built once, where one declared
/// first is maintained for every row the script then writes.
pub(super) fn declared(
    reader: &Session<'_>,
    table: &TableDefinition,
) -> Result<std::result::Result<(String, String), String>> {
    let mut view = reader.store.begin()?;
    let catalog = Catalog::new(&mut view);
    let fields = catalog.fields_on(table.id)?;
    let indexes: Vec<_> = catalog
        .indexes_on(table.id)?
        .into_iter()
        .filter(|index| index.namespace == table.namespace && index.database == table.database)
        .collect();
    let declaration = match crate::describe::declaration(table, &fields, &[]) {
        Ok(text) => text,
        Err(unwritable) => return Ok(Err(unwritable.part)),
    };
    // The word that declared a vector or geo store declared its index too.
    let declared_by_the_word = match table.kind {
        TableKind::Vector(_) => Some(VECTOR_FIELD),
        TableKind::Geo => Some(GEO_FIELD),
        _ => None,
    };
    let mut after = String::new();
    for index in &indexes {
        // A search's member is written as the `DEFINE SEARCH` it belongs to,
        // once per database, after every table it reads (ADR-0105).
        if declared_by_the_word == Some(index.name.as_str())
            || crate::describe::made_by_the_edge_word(table, &index.name)
            || index.engine.is_some()
        {
            continue;
        }
        if let Err(unwritable) = crate::describe::write_index(&mut after, &table.name, index) {
            return Ok(Err(unwritable.part));
        }
    }
    Ok(Ok((declaration, after)))
}

/// What a table's records lose on the way through a script, if anything.
pub(super) fn note(table: &TableDefinition) -> Option<&'static str> {
    match table.kind {
        TableKind::Topic(_) => Some(
            "messages are written again in order and numbered again from 1; \
             reader positions and groups are not carried",
        ),
        TableKind::Queue(_) => Some("holds and attempt counts are not carried"),
        TableKind::Vault(_) => Some("its secrets are not carried"),
        TableKind::Space(_) => Some("an expiry is carried as the time it had left"),
        TableKind::Bucket(_) => Some("a file's `updated` becomes the time it is written again"),
        _ => None,
    }
}

/// Write every record of `table` as the statement its kind is written with, and
/// answer how many.
pub(super) fn records(
    reader: &mut Session<'_>,
    table: &TableDefinition,
    names: &tessari_ql::literal::Names,
    body: &mut String,
) -> Result<u64> {
    let name = &table.name;
    let rows = match reader.run(&format!("SELECT * FROM {name};"))?.pop() {
        Some(Outcome::Records { records, .. }) => records,
        _ => return Ok(0),
    };
    let mut written = 0_u64;
    for (at, (id, value)) in rows.iter().enumerate() {
        if at % BATCH == 0 {
            if at > 0 {
                body.push_str("COMMIT;\n");
            }
            body.push_str("BEGIN;\n");
        }
        let target = format!("{name}:{}", id.to_literal());
        let held = tessari_ql::literal::value(value, names);
        match table.kind {
            TableKind::Bucket(_) => {
                let bytes = match reader.run(&format!("READ {target};"))?.pop() {
                    Some(Outcome::Value(bytes @ Value::Bytes(_))) => bytes,
                    _ => Value::Bytes(Vec::new()),
                };
                let _ = writeln!(
                    body,
                    "PUT {target} = {};",
                    tessari_ql::literal::value(&bytes, names)
                );
            }
            TableKind::Space(_) => {
                let expiry = match reader.run(&format!("RETURN TTL {target};"))?.pop() {
                    Some(Outcome::Value(left @ Value::Duration(_))) => {
                        format!(" EXPIRE {}", tessari_ql::literal::value(&left, names))
                    }
                    _ => String::new(),
                };
                let _ = writeln!(body, "SET {target} = {held}{expiry};");
            }
            _ => {
                let _ = writeln!(body, "CREATE {target} = {held};");
            }
        }
        written = written.saturating_add(1);
    }
    if !rows.is_empty() {
        body.push_str("COMMIT;\n");
    }
    Ok(written)
}
