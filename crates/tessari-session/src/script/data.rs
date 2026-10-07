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
    // Events go after the data too, and here it is a matter of correctness
    // rather than cost: an event declared before the records are written
    // again would run for each of them and apply its effects a second time
    // over the effects the script already carries (ADR-0110 D8).
    for event in &table.events {
        crate::describe::write_event(&mut after, &table.name, event);
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
        TableKind::Space(_) => Some(
            "an expiry is carried as the instant it falls at, and a key whose instant has \
             passed by the time the script runs is not written back",
        ),
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
                let _ = writeln!(body, "SET {target} = {held};");
                write_instant(reader, &target, names, body)?;
            }
            _ => {
                // A table that declares expiry stamps its default on a create, so
                // a record that had no instant says so (ADR-0122 A9).
                let never = if table.expire.is_some() {
                    " EXPIRE NONE"
                } else {
                    ""
                };
                let _ = writeln!(body, "CREATE {target} = {held}{never};");
                write_instant(reader, &target, names, body)?;
            }
        }
        written = written.saturating_add(1);
    }
    if !rows.is_empty() {
        body.push_str("COMMIT;\n");
    }
    Ok(written)
}

/// `EXPIRE <record> <instant>` after a record that carries one — the instant as
/// a datetime rather than the time it had left, so a script run later does not
/// extend every lifetime by the delay (ADR-0122 A9, Q-952). An instant that has
/// passed by then removes the record, which is the verb's own rule, so a script
/// never writes back a record a snapshot of the same moment would hide.
fn write_instant(
    reader: &mut Session<'_>,
    target: &str,
    names: &tessari_ql::literal::Names,
    body: &mut String,
) -> Result<()> {
    let left = match reader.run(&format!("RETURN TTL {target};"))?.pop() {
        Some(Outcome::Value(left @ Value::Duration(_))) => left,
        _ => return Ok(()),
    };
    let (Value::Duration(left), Some(Outcome::Value(Value::Datetime(now)))) =
        (left, reader.run("RETURN time::now();")?.pop())
    else {
        return Ok(());
    };
    let Some(at) = later(now, left) else {
        return Ok(());
    };
    let at = Value::Datetime(at);
    let _ = writeln!(
        body,
        "EXPIRE {target} {};",
        tessari_ql::literal::value(&at, names)
    );
    Ok(())
}

/// `now` moved on by `left`, or `None` past what a datetime holds.
fn later(
    now: tessari_types::Datetime,
    left: tessari_types::Duration,
) -> Option<tessari_types::Datetime> {
    const NANOS_PER_SECOND: u32 = 1_000_000_000;
    let nanos = now.nanos().checked_add(left.nanos())?;
    let (carry, nanos) = if nanos >= NANOS_PER_SECOND {
        (1, nanos.checked_sub(NANOS_PER_SECOND)?)
    } else {
        (0, nanos)
    };
    let seconds = now
        .seconds()
        .checked_add(left.seconds())?
        .checked_add(carry)?;
    tessari_types::Datetime::new(seconds, nanos)
}
