//! COPY both directions over the wire. The statement answers with the copy
//! subprotocol instead of the row flow: FROM streams text rows in as
//! CopyData and lands them as ONE engine INSERT (the batch lands whole or
//! not at all, and a malformed row refuses naming the copy's own line); TO
//! reads through the store and streams the rows out. Text format is the one
//! spoken: tab-separated, `\n`-terminated rows, the null marker matched
//! before backslash escapes run.

use std::sync::Arc;

use rust_decimal::Decimal;
use tessari_types::{Datetime, Number, Value, parse_uuid};
use tessaridb::{Outcome, Parameters};

use crate::codec::{self, Frontend};
use crate::error::Result;

use super::Shared;
use super::answer::{fail_block, refused_frame, sync_block};
use super::connection::Conn;
use super::errors::{Refused, code};
use super::running::{Held, run_refusal};
use super::translate::copy::{Options, Source, Statement};
use super::translate::{self, Columns};

/// One declared field of the table the copy rides on — the kind the text is
/// coerced under, and whether a null may land.
#[derive(Debug, Clone)]
struct Field {
    name: String,
    kind: String,
    required: bool,
    default: bool,
}

/// The read a COPY TO plans: the store script, its bound values, and the
/// projection shape the rows answer under.
struct Planned {
    script: String,
    parameters: Parameters,
    projection: Columns,
    record_names: Vec<Option<String>>,
    table: Option<String>,
    offset: u64,
}

/// Run one COPY statement to its readiness, answering the frames the
/// subprotocol owes.
pub(crate) async fn run(
    conn: &mut Conn,
    statement: Statement,
    tx: &mut codec::write::Tx,
    held: &mut Held,
    shared: &Arc<Shared>,
) -> Result<()> {
    match statement {
        Statement::From {
            table,
            columns,
            options,
        } => from(conn, &table, &columns, &options, tx, held, shared).await,
        Statement::To { source, options } => to(conn, source, &options, tx, held, shared).await,
    }
}

/// `COPY table [(cols…)] FROM STDIN`: answer CopyInResponse, collect the
/// rows, land them as one INSERT in one transaction.
async fn from(
    conn: &mut Conn,
    table: &str,
    wanted: &[String],
    options: &Options,
    tx: &mut codec::write::Tx,
    held: &mut Held,
    shared: &Arc<Shared>,
) -> Result<()> {
    let fields = match table_fields(held, shared, table).await {
        Ok(fields) => fields,
        Err(refused) => return refuse_now(conn, tx, held, shared, refused).await,
    };
    if fields.is_empty() {
        return refuse_now(
            conn,
            tx,
            held,
            shared,
            Refused::surface(
                code::FEATURE,
                format!("COPY wants a table with declared columns in this build: {table}"),
            ),
        )
        .await;
    }
    let columns = match resolve(&fields, wanted, table) {
        Ok(columns) => columns,
        Err(refused) => return refuse_now(conn, tx, held, shared, refused).await,
    };
    conn.send(&codec::write::copy_in_response(columns.len()))
        .await?;
    let mut buffer: Vec<u8> = Vec::new();
    let mut line = 0_usize;
    let mut bound = Parameters::new();
    let mut tuples: Vec<Vec<String>> = Vec::new();
    loop {
        match conn.message().await? {
            Frontend::CopyData(chunk) => {
                buffer.extend_from_slice(&chunk);
                while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                    let raw: Vec<u8> = buffer.drain(..end).collect();
                    let _newline = buffer.remove(0);
                    line = line.saturating_add(1);
                    if options.header && line == 1 {
                        continue;
                    }
                    if let Err(refused) =
                        take_row(&raw, &columns, options, line, &mut bound, &mut tuples)
                    {
                        // The error answers now; the frames the client still
                        // owes are read to the copy's own end, then the
                        // readiness — the protocol's own shape.
                        return refuse_stream(conn, tx, held, shared, refused).await;
                    }
                }
            }
            Frontend::CopyDone => break,
            Frontend::CopyFail(message) => {
                return refuse_now(
                    conn,
                    tx,
                    held,
                    shared,
                    Refused::with_detail(code::BAD_COPY, message, "the client aborted the copy"),
                )
                .await;
            }
            Frontend::Terminate => return Ok(()),
            _other => {
                return refuse_stream(
                    conn,
                    tx,
                    held,
                    shared,
                    Refused::surface(code::PROTOCOL, "COPY wants CopyData, CopyDone or CopyFail"),
                )
                .await;
            }
        }
    }
    if !buffer.is_empty() {
        line = line.saturating_add(1);
        return refuse_now(
            conn,
            tx,
            held,
            shared,
            Refused::with_detail(
                code::BAD_COPY,
                format!("COPY line {line} is not newline-terminated"),
                "the final row wants a newline",
            ),
        )
        .await;
    }
    if tuples.is_empty() {
        conn.send(&codec::write::command_complete("COPY 0")).await?;
        conn.send(&codec::write::ready_for_query(*tx)).await?;
        return Ok(());
    }
    let columns_sql = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let rows_sql = tuples
        .iter()
        .map(|names| format!("({})", names.join(", ")))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!("INSERT INTO {table} ({columns_sql}) VALUES {rows_sql};");
    match held.run(shared, &script, &bound).await {
        Ok(_) => {}
        // The batch is one statement: a refusal takes it whole, nothing lands.
        Err(failed) => return refuse_now(conn, tx, held, shared, run_refusal(failed)).await,
    }
    sync_block(tx, held);
    conn.send(&codec::write::command_complete(&format!(
        "COPY {}",
        tuples.len()
    )))
    .await?;
    conn.send(&codec::write::ready_for_query(*tx)).await?;
    Ok(())
}

/// Decode one buffered line into the batch: the null marker matched raw,
/// escapes processed, arity checked, every value coerced under its column's
/// kind — each refusal naming the copy's own line.
fn take_row(
    raw: &[u8],
    columns: &[Field],
    options: &Options,
    line: usize,
    bound: &mut Parameters,
    tuples: &mut Vec<Vec<String>>,
) -> std::result::Result<(), Refused> {
    let mut cells = Vec::new();
    for field in raw.split(|byte| *byte == options.delimiter) {
        // The null marker matches the raw cell, before any escape runs.
        if field == options.null.as_bytes() {
            cells.push(None);
            continue;
        }
        cells.push(Some(unescape(field, line)?));
    }
    if cells.len() != columns.len() {
        return Err(Refused::with_detail(
            code::BAD_COPY,
            format!(
                "COPY row {line}: {} columns, {} values",
                columns.len(),
                cells.len()
            ),
            "one value per column",
        ));
    }
    let mut names = Vec::with_capacity(cells.len());
    for (column, cell) in columns.iter().zip(cells) {
        let name = format!("v{}", bound.len());
        let value = match cell {
            None => {
                if column.required && !column.default {
                    return Err(Refused::with_detail(
                        code::NOT_NULL,
                        format!(
                            "null value in column {} of the copy: the field is strict",
                            column.name
                        ),
                        format!("COPY line {line}: SchemaViolation"),
                    ));
                }
                Value::Null
            }
            Some(text) => coerce(column, &text, line)?,
        };
        bound.insert(name.clone(), value);
        names.push(format!("${name}"));
    }
    tuples.push(names);
    Ok(())
}

/// The escape processing of the dialect's text format: `\b \f \n \r \t \v`,
/// one to three octal digits, one or two hex digits after `\x`, a doubled
/// backslash, and any other backslashed character is itself.
fn unescape(field: &[u8], line: usize) -> std::result::Result<String, Refused> {
    let malformed = |detail: &str| {
        Refused::with_detail(
            code::BAD_COPY,
            format!("COPY row {line} is malformed"),
            detail,
        )
    };
    let digit = |byte: u8| -> Option<u16> {
        char::from(byte)
            .to_digit(16)
            .and_then(|held| u16::try_from(held).ok())
    };
    let mut out = Vec::with_capacity(field.len());
    let mut at = 0_usize;
    while let Some(byte) = field.get(at) {
        if *byte != b'\\' {
            out.push(*byte);
            at = at.saturating_add(1);
            continue;
        }
        at = at.saturating_add(1);
        let Some(escaped) = field.get(at) else {
            return Err(malformed("a backslash wants what it escapes"));
        };
        match escaped {
            b'b' => {
                out.push(8);
                at = at.saturating_add(1);
            }
            b'f' => {
                out.push(12);
                at = at.saturating_add(1);
            }
            b'n' => {
                out.push(10);
                at = at.saturating_add(1);
            }
            b'r' => {
                out.push(13);
                at = at.saturating_add(1);
            }
            b't' => {
                out.push(9);
                at = at.saturating_add(1);
            }
            b'v' => {
                out.push(11);
                at = at.saturating_add(1);
            }
            b'\\' => {
                out.push(b'\\');
                at = at.saturating_add(1);
            }
            b'0'..=b'7' => {
                let mut value = 0_u16;
                let mut digits = 0_usize;
                while digits < 3
                    && let Some(octal) = field.get(at).and_then(|byte| {
                        char::from(*byte)
                            .to_digit(8)
                            .and_then(|held| u16::try_from(held).ok())
                    })
                {
                    value = value
                        .checked_mul(8)
                        .and_then(|held| held.checked_add(octal))
                        .ok_or_else(|| malformed("an octal escape overflows a byte"))?;
                    at = at.saturating_add(1);
                    digits = digits.saturating_add(1);
                }
                let byte = u8::try_from(value)
                    .map_err(|_| malformed("an octal escape overflows a byte"))?;
                out.push(byte);
            }
            b'x' => {
                at = at.saturating_add(1);
                let mut value = 0_u16;
                let mut digits = 0_usize;
                while digits < 2
                    && let Some(hex) = field.get(at)
                    && let Some(held) = digit(*hex)
                {
                    value = value
                        .checked_mul(16)
                        .and_then(|acc| acc.checked_add(held))
                        .ok_or_else(|| malformed("a hex escape overflows a byte"))?;
                    at = at.saturating_add(1);
                    digits = digits.saturating_add(1);
                }
                if digits == 0 {
                    return Err(malformed("the hex escape wants a digit"));
                }
                let byte =
                    u8::try_from(value).map_err(|_| malformed("a hex escape overflows a byte"))?;
                out.push(byte);
            }
            other => {
                out.push(*other);
                at = at.saturating_add(1);
            }
        }
    }
    String::from_utf8(out).map_err(|_| malformed("the row is not utf-8"))
}

/// The value one cell becomes under its column's declared kind — a refusal
/// names the line and the column, invalid-text coded.
fn coerce(column: &Field, text: &str, line: usize) -> std::result::Result<Value, Refused> {
    let invalid = Refused::with_detail(
        code::INVALID_TEXT,
        format!(
            "COPY line {line}, column {}: {text:?} does not read as {}",
            column.name, column.kind
        ),
        format!("copy line {line}"),
    );
    match column.kind.as_str() {
        "int" => text
            .parse::<i64>()
            .map(|held| Value::Number(held.into()))
            .map_err(|_| invalid),
        "float" => text
            .parse::<f64>()
            .map(|held| Value::Number(Number::float(held)))
            .map_err(|_| invalid),
        "decimal" | "number" => text
            .parse::<Decimal>()
            .map(|held| Value::Number(Number::Decimal(held)))
            .map_err(|_| invalid),
        "bool" => parse_bool(text).map(Value::Bool).ok_or(invalid),
        "string" => Ok(Value::String(text.to_owned())),
        "datetime" => Datetime::parse_rfc3339(text)
            .map(Value::Datetime)
            .ok_or(invalid),
        "uuid" => parse_uuid(text).map(Value::Uuid).ok_or(invalid),
        "bytes" => parse_bytes(text).map(Value::Bytes).ok_or(invalid),
        other => Err(Refused::with_detail(
            code::FEATURE,
            format!(
                "COPY speaks the fixed scalar columns in this build; column {} is {other}",
                column.name
            ),
            format!("copy line {line}"),
        )),
    }
}

/// The dialect's boolean spellings, folded.
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "t" | "true" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// The text form of a bytea value: `\x` followed by whole hex bytes.
fn parse_bytes(text: &str) -> Option<Vec<u8>> {
    let hex = text.strip_prefix("\\x")?;
    let bytes = hex.as_bytes();
    let nibble = |byte: u8| -> Option<u8> {
        char::from(byte)
            .to_digit(16)
            .and_then(|held| u8::try_from(held).ok())
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut at = 0_usize;
    while let (Some(high), Some(low)) = (
        bytes.get(at).and_then(|byte| nibble(*byte)),
        bytes
            .get(at.saturating_add(1))
            .and_then(|byte| nibble(*byte)),
    ) {
        out.push(high.checked_mul(16)?.checked_add(low)?);
        at = at.saturating_add(2);
    }
    // A lone nibble or a character that is not hex refuses the value.
    if at != bytes.len() {
        return None;
    }
    Some(out)
}

/// `COPY … TO STDOUT`: plan the read, answer CopyOutResponse, stream the
/// rows as CopyData, and close with the honest count.
async fn to(
    conn: &mut Conn,
    source: Source,
    options: &Options,
    tx: &mut codec::write::Tx,
    held: &mut Held,
    shared: &Arc<Shared>,
) -> Result<()> {
    let planned = match plan(source, held, shared).await {
        Ok(planned) => planned,
        Err(refused) => return refuse_now(conn, tx, held, shared, refused).await,
    };
    let Planned {
        script,
        parameters,
        projection,
        record_names,
        table,
        offset,
    } = planned;
    let outcomes = match held.run(shared, &script, &parameters).await {
        Ok(outcomes) => outcomes,
        Err(failed) => return refuse_now(conn, tx, held, shared, run_refusal(failed)).await,
    };
    sync_block(tx, held);
    let Some(Outcome::Records { records, .. }) = outcomes.into_iter().next() else {
        return refuse_now(
            conn,
            tx,
            held,
            shared,
            Refused::surface(
                code::INTERNAL,
                "the copy source did not answer with records",
            ),
        )
        .await;
    };
    // A query form may carry an OFFSET head: the store answered the wider
    // bounded read, the copy drops the front — the row flow's own paging.
    let skipped = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(records.len());
    let records = &records[skipped..];
    let (columns, rows) = super::rows(records, &projection, &record_names, table.as_deref());
    conn.send(&codec::write::copy_out_response(columns.len()))
        .await?;
    if options.header {
        let names = columns
            .iter()
            .map(|column| Some(column.name.clone()))
            .collect::<Vec<_>>();
        conn.send(&codec::write::copy_data(&encode_row(&names, options)))
            .await?;
    }
    for row in &rows {
        let cells = row
            .iter()
            .map(|value| value.as_ref().and_then(super::types::text_of))
            .collect::<Vec<_>>();
        conn.send(&codec::write::copy_data(&encode_row(&cells, options)))
            .await?;
    }
    conn.send(&codec::write::copy_done()).await?;
    conn.send(&codec::write::command_complete(&format!(
        "COPY {}",
        rows.len()
    )))
    .await?;
    conn.send(&codec::write::ready_for_query(*tx)).await?;
    Ok(())
}

/// Plan the read a COPY TO runs: the table form names its columns from the
/// catalog (or answers every declared field), the query form rides the usual
/// SELECT translator.
async fn plan(
    source: Source,
    held: &mut Held,
    shared: &Arc<Shared>,
) -> std::result::Result<Planned, Refused> {
    match source {
        Source::Table { table, columns } => {
            let fields = table_fields(held, shared, &table).await?;
            let projection = if columns.is_empty() {
                Columns::Star
            } else {
                Columns::Named(
                    resolve(&fields, &columns, &table)?
                        .iter()
                        .map(|field| field.name.clone())
                        .collect(),
                )
            };
            let chosen = match &projection {
                Columns::Named(names) => names.join(", "),
                Columns::Star => "*".to_owned(),
            };
            Ok(Planned {
                script: format!("SELECT {chosen} FROM {table};"),
                parameters: Parameters::new(),
                projection,
                record_names: Vec::new(),
                table: Some(table),
                offset: 0,
            })
        }
        Source::Query(text) => {
            let translated = translate::translate(&text)?;
            if translated
                .statements
                .iter()
                .any(|shape| shape.catalog.is_some())
            {
                return Err(Refused::surface(
                    code::FEATURE,
                    "COPY (query) over the catalog is not spoken in this build",
                ));
            }
            let Some(shape) = translated.statements.first() else {
                return Err(Refused::surface(
                    code::SYNTAX,
                    "COPY (query) wants one read",
                ));
            };
            Ok(Planned {
                script: translated.script.clone(),
                parameters: translated.parameters.clone(),
                projection: shape.columns.clone(),
                record_names: shape.record_names.clone(),
                table: shape.table.clone(),
                offset: shape.offset,
            })
        }
    }
}

/// The declared fields of a table, in the catalog's own order.
async fn table_fields(
    held: &mut Held,
    shared: &Arc<Shared>,
    table: &str,
) -> std::result::Result<Vec<Field>, Refused> {
    let outcomes = held
        .run(
            shared,
            &format!("INFO FOR TABLE {table};"),
            &Parameters::new(),
        )
        .await
        .map_err(run_refusal)?;
    let Some(Outcome::Value(Value::Object(report))) = outcomes.first() else {
        return Err(Refused::surface(
            code::INTERNAL,
            "the store's table report did not read",
        ));
    };
    let Some(Value::Array(fields)) = report.get("fields") else {
        return Ok(Vec::new());
    };
    Ok(fields.iter().filter_map(field).collect())
}

/// One declared field out of the table report's own shape.
fn field(value: &Value) -> Option<Field> {
    let Value::Object(described) = value else {
        return None;
    };
    let Some(Value::String(name)) = described.get("name") else {
        return None;
    };
    let kind = match described.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        _ => String::new(),
    };
    Some(Field {
        name: name.clone(),
        kind,
        required: matches!(described.get("required"), Some(Value::Bool(true))),
        default: matches!(described.get("default"), Some(Value::String(_))),
    })
}

/// The columns the copy speaks: the named ones, checked against the
/// declaration, or every declared field in the catalog's own order.
fn resolve(
    fields: &[Field],
    wanted: &[String],
    table: &str,
) -> std::result::Result<Vec<Field>, Refused> {
    if wanted.is_empty() {
        return Ok(fields.to_vec());
    }
    wanted
        .iter()
        .map(|name| {
            fields
                .iter()
                .find(|field| field.name == *name)
                .cloned()
                .ok_or_else(|| {
                    Refused::with_detail(
                        code::UNDEFINED_COLUMN,
                        format!("column {name} of COPY is not a column of {table}"),
                        format!("column {name}"),
                    )
                })
        })
        .collect()
}

/// One row in the text format: cells joined by the delimiter, the null
/// marker for absent values, a newline at the row's end.
fn encode_row(cells: &[Option<String>], options: &Options) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push(options.delimiter);
        }
        match cell {
            None => out.extend_from_slice(options.null.as_bytes()),
            Some(text) => escape(text, options.delimiter, &mut out),
        }
    }
    out.push(b'\n');
    out
}

/// The output escapes: backslash, newline, carriage return, and the
/// delimiter itself (a tab rides as `\t`, another byte as backslash plus
/// itself).
fn escape(text: &str, delimiter: u8, out: &mut Vec<u8>) {
    for byte in text.bytes() {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ if byte == delimiter => {
                out.push(b'\\');
                if delimiter == b'\t' {
                    out.push(b't');
                } else {
                    out.push(delimiter);
                }
            }
            _ => out.push(byte),
        }
    }
}

/// A refusal after the copy ended (or never began): the block aborts, the
/// error and the readiness answer together — a simple query's own shape.
async fn refuse_now(
    conn: &mut Conn,
    tx: &mut codec::write::Tx,
    held: &mut Held,
    shared: &Arc<Shared>,
    refused: Refused,
) -> Result<()> {
    fail_block(tx, held, shared).await;
    conn.refuse(&refused, *tx).await
}

/// A refusal mid-copy: the error answers at once, then the frames the client
/// still owes are read to the copy's own end — CopyData drops on the floor,
/// the readiness answers after CopyDone or CopyFail.
async fn refuse_stream(
    conn: &mut Conn,
    tx: &mut codec::write::Tx,
    held: &mut Held,
    shared: &Arc<Shared>,
    refused: Refused,
) -> Result<()> {
    fail_block(tx, held, shared).await;
    conn.send(&refused_frame(&refused)).await?;
    loop {
        match conn.message().await? {
            Frontend::CopyData(_) => {}
            Frontend::CopyDone | Frontend::CopyFail(_) | Frontend::Terminate => break,
            // Not a copy frame: the copy is over by the error's own hand,
            // and nothing further of it is owed.
            _other => break,
        }
    }
    conn.send(&codec::write::ready_for_query(*tx)).await
}
