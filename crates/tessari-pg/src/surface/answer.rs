//! The answering machinery: one statement's outcome as the wire spells
//! it — rows folded and encoded, completions tagged, transaction verbs
//! driven, refusals framed. Split from `connection` (LR-DB-025): the
//! connection owns the loop and the stream; this module owns what an answer
//! says. Everything here is a free function over `&mut Conn` — the seam is
//! behavior, not state.

use std::sync::Arc;

use crate::codec;
use crate::error::Result;

use super::Shared;
use super::connection::Conn;
use super::errors::{Refused, code};
use super::translate;
use super::types;

pub(crate) async fn answer_catalog(
    conn: &mut Conn,
    query: &translate::catalog::Query,
    rows: &[Vec<Option<tessaridb::Value>>],
    described: bool,
    formats: &[u16],
) -> Result<bool> {
    let mut columns = query.columns();
    let encoded = match super::encode(&columns, rows, formats) {
        Ok(encoded) => encoded,
        Err(refused) => {
            conn.refuse_pending(&refused).await?;
            return Ok(false);
        }
    };
    if !described {
        if let Ok(expanded) = super::expand_formats(formats, columns.len()) {
            for (i, column) in columns.iter_mut().enumerate() {
                column.format = expanded[i];
            }
        }
        conn.send(&codec::write::row_description(&columns)).await?;
    }
    for row in &encoded {
        conn.send(&codec::write::data_row(row)).await?;
    }
    conn.send(&codec::write::command_complete(&format!(
        "SELECT {}",
        rows.len()
    )))
    .await?;
    Ok(true)
}

/// One transaction verb, driven through the store's own block: `BEGIN` parks
/// an open transaction in the session, `COMMIT` lands it, `ROLLBACK` answers
/// in the store's own word (`CANCEL`). What remains connection-local is the
/// aborted state: a failure inside the block drops what the session held, so
/// the two closing verbs of an aborted block answer here without touching the
/// store — and `COMMIT` of a healthy block that the store refuses reports the
/// refusal and closes, as the dialect counts.
pub(crate) async fn transaction(
    conn: &mut Conn,
    verb: super::session::TxVerb,
    tx: &mut codec::write::Tx,
    held: &mut super::running::Held,
    shared: &Arc<Shared>,
    ready: bool,
) -> Result<()> {
    use super::session::TxVerb;
    match verb {
        TxVerb::Begin => match tx {
            codec::write::Tx::Idle => {
                let reply = match held
                    .run(shared, "BEGIN", &tessaridb::Parameters::new())
                    .await
                {
                    Ok(_) => {
                        *tx = codec::write::Tx::InTx;
                        Ok("BEGIN")
                    }
                    Err(failed) => Err(super::running::run_refusal(failed)),
                };
                reply_tag(conn, reply).await?;
            }
            _ => {
                conn.send(&codec::write::notice(
                    code::NO_TRANSACTION,
                    "there is already a transaction in progress",
                ))
                .await?;
                conn.send(&codec::write::command_complete("BEGIN")).await?;
            }
        },
        TxVerb::Commit => match tx {
            codec::write::Tx::InTx => {
                let reply = match held
                    .run(shared, "COMMIT", &tessaridb::Parameters::new())
                    .await
                {
                    Ok(_) => {
                        *tx = codec::write::Tx::Idle;
                        Ok("COMMIT")
                    }
                    Err(failed) => {
                        // The store refused the commit; the block it held is
                        // gone either way.
                        *tx = codec::write::Tx::Idle;
                        Err(super::running::run_refusal(failed))
                    }
                };
                reply_tag(conn, reply).await?;
            }
            codec::write::Tx::Failed => {
                *tx = codec::write::Tx::Idle;
                conn.send(&codec::write::command_complete("ROLLBACK"))
                    .await?;
            }
            codec::write::Tx::Idle => {
                refuse_tag(
                    conn,
                    Refused::surface(code::NO_TRANSACTION, "there is no transaction in progress"),
                )
                .await?;
            }
        },
        TxVerb::Rollback => match tx {
            codec::write::Tx::InTx => {
                let reply = match held
                    .run(shared, "CANCEL", &tessaridb::Parameters::new())
                    .await
                {
                    Ok(_) => {
                        *tx = codec::write::Tx::Idle;
                        Ok("ROLLBACK")
                    }
                    Err(failed) => {
                        if !held.transaction_open() {
                            *tx = codec::write::Tx::Idle;
                        }
                        Err(super::running::run_refusal(failed))
                    }
                };
                reply_tag(conn, reply).await?;
            }
            codec::write::Tx::Failed => {
                *tx = codec::write::Tx::Idle;
                conn.send(&codec::write::command_complete("ROLLBACK"))
                    .await?;
            }
            codec::write::Tx::Idle => {
                refuse_tag(
                    conn,
                    Refused::surface(code::NO_TRANSACTION, "there is no transaction in progress"),
                )
                .await?;
            }
        },
    }
    // The simple query owes the readiness with the answer; the extended
    // protocol's Sync owns it there, and a verb's Execute answers the tag
    // alone.
    if ready {
        conn.send(&codec::write::ready_for_query(*tx)).await?;
    }
    Ok(())
}

/// A completion tag or a refusal as the wire spells it — the shared tail of
/// the verb handler.
async fn reply_tag(conn: &mut Conn, reply: std::result::Result<&str, Refused>) -> Result<()> {
    match reply {
        Ok(tag) => conn.send(&codec::write::command_complete(tag)).await?,
        Err(refused) => {
            conn.send(&codec::write::error(
                refused.code,
                &refused.message,
                refused.detail.as_deref(),
            ))
            .await?;
        }
    }
    Ok(())
}

/// A refusal with no tag to follow — the verb handler's refusal tail.
async fn refuse_tag(conn: &mut Conn, refused: Refused) -> Result<()> {
    conn.send(&codec::write::error(
        refused.code,
        &refused.message,
        refused.detail.as_deref(),
    ))
    .await?;
    Ok(())
}

/// Enter the aborted state: the block the connection was riding is dropped
/// engine-side too, so nothing stays parked behind a Failed readiness. Runs
/// before every refusal a statement can meet inside a block.
pub(crate) async fn fail_block(
    tx: &mut codec::write::Tx,
    held: &mut super::running::Held,
    shared: &Arc<Shared>,
) {
    if *tx == codec::write::Tx::InTx {
        *tx = codec::write::Tx::Failed;
        // The refusal aborts the block; the discard's own refusal, should one
        // arrive, changes nothing the client has not already been told.
        let _discard = held
            .run(shared, "CANCEL", &tessaridb::Parameters::new())
            .await;
    }
}

/// After a statement ran, reconcile the connection's block state with what
/// the session actually holds: a statement string that closed the block
/// (a verb among its statements) drops the readiness to idle, one that opened
/// one raises it.
pub(crate) fn sync_block(tx: &mut codec::write::Tx, held: &super::running::Held) {
    *tx = match (*tx, held.transaction_open()) {
        (codec::write::Tx::InTx, false) => codec::write::Tx::Idle,
        (codec::write::Tx::Idle, true) => codec::write::Tx::InTx,
        (state, _) => state,
    };
}

/// One statement's outcome as the wire spells it. `described` marks that a
/// Describe(portal) already answered the first records' row description, so
/// the Execute owes only the rows and the tag.
/// One statement's outcome as the wire spells it. `described` marks that a
/// Describe(portal) already answered the first records' row description, so
/// the Execute owes only the rows and the tag. `formats` are what a Bind
/// asked: text everywhere, or the binary subset per column. Answers `false`
/// when an encoding refusal was sent instead — the caller owns the
/// conversation state (readiness, or the skip that follows an extended
/// error).
pub(crate) async fn answer(
    conn: &mut Conn,
    outcome: &tessaridb::Outcome,
    shape: &translate::Shaped,
    described: bool,
    formats: &[u16],
) -> Result<bool> {
    use tessaridb::Outcome;
    // A statement carrying a WARNING answers it beside the completion — the
    // dropped column's kept values ride here, before whatever the outcome
    // owes.
    if let Some(notice) = &shape.notice {
        conn.send(&codec::write::notice("00000", notice)).await?;
    }
    match outcome {
        Outcome::Records { records, .. } => {
            // A write the client did not RETURN owes only the count — no
            // rows, no description, the tag naming how many.
            if shape.tag != translate::write::Tag::Select && shape.returning.is_none() {
                conn.send(&codec::write::command_complete(
                    &shape.tag.completion(records.len()),
                ))
                .await?;
                return Ok(true);
            }
            // The OFFSET head drops before the fold: the store answered the
            // wider bounded read, the port pages it by discarding the front —
            // the cost is the port's, the answer is exact.
            let skipped = usize::try_from(shape.offset)
                .unwrap_or(usize::MAX)
                .min(records.len());
            let records = &records[skipped..];
            let (mut columns, rows) = if records.is_empty()
                && let translate::Columns::Named(names) = &shape.columns
            {
                // An empty answer still owes the client the projection's
                // names: the rows cannot speak for columns that never came.
                let columns = names
                    .iter()
                    .map(|name| codec::write::Column {
                        name: name.clone(),
                        type_oid: types::oid::TEXT,
                        type_len: -1,
                        format: 0,
                    })
                    .collect::<Vec<_>>();
                (columns, Vec::new())
            } else {
                super::rows(
                    records,
                    &shape.columns,
                    &shape.record_names,
                    shape.table.as_deref(),
                )
            };
            let encoded = match super::encode(&columns, &rows, formats) {
                Ok(encoded) => encoded,
                Err(refused) => {
                    conn.refuse_pending(&refused).await?;
                    return Ok(false);
                }
            };
            if !described {
                // The columns answer under the formats a Bind asked for, one
                // word naming every column, a word per column naming each.
                if let Ok(expanded) = super::expand_formats(formats, columns.len()) {
                    for (i, column) in columns.iter_mut().enumerate() {
                        column.format = expanded[i];
                    }
                }
                conn.send(&codec::write::row_description(&columns)).await?;
            }
            for row in &encoded {
                conn.send(&codec::write::data_row(row)).await?;
            }
            conn.send(&codec::write::command_complete(
                &shape.tag.completion(rows.len()),
            ))
            .await?;
        }
        Outcome::Value(value) => {
            // A write the store answers as one value (CREATE's RETURN AFTER):
            // the tag names it, or the RETURNING projection answers under
            // the record's own fields.
            if shape.tag != translate::write::Tag::Select {
                return answer_write_value(conn, shape, value, described, formats).await;
            }
            let mut column = super::value_column(value);
            if let Ok(expanded) = super::expand_formats(formats, 1)
                && let Some(format) = expanded.first()
            {
                column.format = *format;
            }
            conn.send(&codec::write::row_description(std::slice::from_ref(
                &column,
            )))
            .await?;
            let encoded = match super::encode(
                std::slice::from_ref(&column),
                &[vec![Some(value.clone())]],
                formats,
            ) {
                Ok(encoded) => encoded,
                Err(refused) => {
                    conn.refuse_pending(&refused).await?;
                    return Ok(false);
                }
            };
            for row in &encoded {
                conn.send(&codec::write::data_row(row)).await?;
            }
            conn.send(&codec::write::command_complete("SELECT 1"))
                .await?;
        }
        Outcome::Done => {
            conn.send(&codec::write::command_complete(&shape.tag.completion(0)))
                .await?;
        }
        other => {
            conn.refuse_pending(&Refused::with_detail(
                code::INTERNAL,
                "this answer shape is not spoken in this build",
                format!("{other:?}"),
            ))
            .await?;
            return Ok(false);
        }
    }
    Ok(true)
}

/// One write's single value as the wire spells it: no RETURNING owes the
/// tag alone; RETURNING answers the value's fields under the projection —
/// a star takes the record's own field order, a name list takes those.
async fn answer_write_value(
    conn: &mut Conn,
    shape: &translate::Shaped,
    value: &tessaridb::Value,
    described: bool,
    formats: &[u16],
) -> Result<bool> {
    if shape.returning.is_none() {
        conn.send(&codec::write::command_complete(&shape.tag.completion(1)))
            .await?;
        return Ok(true);
    }
    let (columns, rows) = match (&shape.columns, value) {
        (translate::Columns::Star, tessaridb::Value::Object(fields)) => {
            let columns = fields
                .keys()
                .map(|name| codec::write::Column {
                    name: name.clone(),
                    type_oid: types::oid::TEXT,
                    type_len: -1,
                    format: 0,
                })
                .collect::<Vec<_>>();
            let row = fields.values().map(|held| Some(held.clone())).collect();
            (columns, vec![row])
        }
        (translate::Columns::Named(names), tessaridb::Value::Object(fields)) => {
            let columns = names
                .iter()
                .map(|name| codec::write::Column {
                    name: name.clone(),
                    type_oid: types::oid::TEXT,
                    type_len: -1,
                    format: 0,
                })
                .collect::<Vec<_>>();
            let row = names
                .iter()
                .map(|name| fields.get(name).cloned().or(Some(tessaridb::Value::None)))
                .collect();
            (columns, vec![row])
        }
        _ => (Vec::new(), vec![vec![Some(value.clone())]]),
    };
    let encoded = match super::encode(&columns, &rows, formats) {
        Ok(encoded) => encoded,
        Err(refused) => {
            conn.refuse_pending(&refused).await?;
            return Ok(false);
        }
    };
    if !described {
        let mut columns = columns;
        if let Ok(expanded) = super::expand_formats(formats, columns.len()) {
            for (i, column) in columns.iter_mut().enumerate() {
                column.format = expanded[i];
            }
        }
        conn.send(&codec::write::row_description(&columns)).await?;
    }
    for row in &encoded {
        conn.send(&codec::write::data_row(row)).await?;
    }
    conn.send(&codec::write::command_complete(
        &shape.tag.completion(rows.len()),
    ))
    .await?;
    Ok(true)
}

/// The frame an error refuses with.
pub(crate) fn refused_frame(refused: &Refused) -> Vec<u8> {
    codec::write::error(refused.code, &refused.message, refused.detail.as_deref())
}
