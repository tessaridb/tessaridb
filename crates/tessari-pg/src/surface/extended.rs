//! The extended query protocol: Parse/Bind/Describe/Execute/Sync cycles over
//! named statements and portals, with the error-skip-to-Sync recovery the
//! dialect's clients build on. A statement is translated once at Parse; its
//! `$n` placeholders take the values a Bind fills; a Describe answers what the
//! wire owes without hiding what this build does not speak.

use std::collections::HashMap;
use std::sync::Arc;

use tessaridb::{Outcome, Parameters};

use crate::codec::{self, Frontend};
use crate::error::Result;

use super::connection::{Conn, answer};
use super::errors::{Refused, code};
use super::running::{Failed, Held};
use super::translate::{self, Columns};
use super::types;

/// The most named statements or portals one connection holds open.
const MAX_NAMED: usize = 1024;

/// The extended-protocol state of one connection. One connection is one task,
/// so a plain map is the right hold — nothing here crosses a task boundary.
pub(crate) struct Kitchen {
    statements: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// An extended error was answered; every message is dropped until the
    /// Sync that re-synchronises the conversation.
    skipping: bool,
    /// The transaction block the readiness reports, shared with the simple
    /// query path through [`Self::tx`].
    pub(crate) tx: codec::write::Tx,
}

/// A parsed statement: its translation and the parameter types it declared.
struct Prepared {
    translated: translate::Translated,
    types: Vec<u32>,
}

/// A bound portal: the statement's shape, the values its placeholders took,
/// and what a Describe already ran so an Execute replays instead of re-runs.
struct Portal {
    translated: translate::Translated,
    parameters: Parameters,
    /// The result formats the Bind asked for: one word names every column,
    /// a word per column names each, nothing asks for text.
    formats: Vec<u16>,
    /// The outcomes a Describe(portal) already produced.
    spent: Option<Vec<Outcome>>,
    /// A Describe already answered this portal's row description.
    described: bool,
    /// An Execute already drained the portal; the dialect answers a spent
    /// portal with an empty completion, never a re-run.
    done: bool,
    /// The rows a Describe(portal) already evaluated for a catalog statement —
    /// answered locally, never against the store.
    catalog_rows: Option<Vec<Vec<Option<tessaridb::Value>>>>,
}

impl Kitchen {
    pub(crate) fn new() -> Self {
        Self {
            statements: HashMap::new(),
            portals: HashMap::new(),
            skipping: false,
            tx: codec::write::Tx::Idle,
        }
    }

    /// One message of the extended protocol. A refusal answers the message and
    /// drops everything until Sync; Sync re-opens the conversation.
    pub(crate) async fn handle(
        &mut self,
        conn: &mut Conn,
        message: Frontend,
        held: &mut Held,
        shared: &Arc<super::Shared>,
    ) -> Result<()> {
        match message {
            Frontend::Parse { name, query, types } => self.parse(conn, &name, &query, &types).await,
            Frontend::Bind {
                portal,
                statement,
                formats,
                values,
                results,
            } => {
                self.bind(conn, &portal, &statement, &formats, &values, &results)
                    .await
            }
            Frontend::Describe { kind, name } => {
                self.describe(conn, held, shared, kind, &name).await
            }
            Frontend::Execute { portal, rows } => {
                self.execute(conn, held, shared, &portal, rows).await
            }
            Frontend::Close { kind, name } => self.close(conn, kind, &name).await,
            Frontend::Sync => self.sync(conn).await,
            // Flush asks for delivery; this surface holds nothing back, so the
            // question answers itself — also while skipping.
            Frontend::Flush => Ok(()),
            other => {
                debug_assert!(false, "the caller routes the extended set here: {other:?}");
                Ok(())
            }
        }
    }

    /// A simple Query arrived: it re-synchronises the conversation, so a
    /// pending skip ends and the unnamed portal closes, as after a Sync.
    pub(crate) fn interrupt(&mut self) {
        self.skipping = false;
        self.portals.remove("");
    }

    async fn parse(
        &mut self,
        conn: &mut Conn,
        name: &str,
        query: &str,
        types: &[u32],
    ) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        let refusal = match translate::translate(query) {
            Err(refused) => Some(refused),
            Ok(translated) => {
                if !types.is_empty() && translated.slots.len() > types.len() {
                    Some(Refused::with_detail(
                        code::SYNTAX,
                        "the query uses more parameters than the Parse declared",
                        format!("uses ${}, declared {}", translated.slots.len(), types.len()),
                    ))
                } else if name.is_empty() || self.statements.len() < MAX_NAMED {
                    let mut resolved = types.to_vec();
                    if let Some(query) = translated
                        .statements
                        .first()
                        .and_then(|shape| shape.catalog.as_ref())
                    {
                        for (index, inferred) in query.param_types.iter().enumerate() {
                            if resolved.get(index).copied().unwrap_or(0) == 0 {
                                resolved.resize(index.saturating_add(1).max(resolved.len()), 0);
                                resolved[index] = *inferred;
                            }
                        }
                    }
                    self.statements.insert(
                        name.to_owned(),
                        Prepared {
                            translated,
                            types: resolved,
                        },
                    );
                    conn.send(&codec::write::parse_complete()).await?;
                    None
                } else {
                    Some(Refused::surface(
                        code::LIMIT,
                        "this connection holds as many named statements as it takes",
                    ))
                }
            }
        };
        if let Some(refused) = refusal {
            self.fail(conn, &refused).await?;
        }
        Ok(())
    }

    async fn bind(
        &mut self,
        conn: &mut Conn,
        portal: &str,
        statement: &str,
        formats: &[u16],
        values: &[Option<Vec<u8>>],
        results: &[u16],
    ) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        let refusal = self.prepare_bind(portal, statement, formats, values, results);
        match refusal {
            Ok(bound) => {
                self.portals.insert(portal.to_owned(), bound);
                conn.send(&codec::write::bind_complete()).await?;
            }
            Err(refused) => self.fail(conn, &refused).await?,
        }
        Ok(())
    }

    fn prepare_bind(
        &mut self,
        portal: &str,
        statement: &str,
        formats: &[u16],
        values: &[Option<Vec<u8>>],
        results: &[u16],
    ) -> std::result::Result<Portal, Refused> {
        let prepared = self.statements.get(statement).ok_or_else(|| {
            Refused::with_detail(
                code::UNDEFINED_STATEMENT,
                "the statement was not prepared on this connection",
                format!("name {statement:?}"),
            )
        })?;
        let param_formats = super::expand_formats(formats, values.len()).map_err(|_| {
            Refused::with_detail(
                code::PROTOCOL,
                "the parameter formats do not match the Bind's values",
                format!("{} formats for {} values", formats.len(), values.len()),
            )
        })?;
        for format in param_formats.iter().chain(results.iter()) {
            if *format > 1 {
                return Err(Refused::surface(
                    code::PROTOCOL,
                    "a format other than text or binary is not spoken",
                ));
            }
        }
        let slots = &prepared.translated.slots;
        if values.len() != slots.len() {
            return Err(Refused::with_detail(
                code::PROTOCOL,
                "the Bind's values do not match the statement's parameters",
                format!("expected {}, got {}", slots.len(), values.len()),
            ));
        }
        if !portal.is_empty() && self.portals.len() >= MAX_NAMED {
            return Err(Refused::surface(
                code::LIMIT,
                "this connection holds as many named portals as it takes",
            ));
        }
        let mut parameters = prepared.translated.parameters.clone();
        for (index, value) in values.iter().enumerate() {
            let Some(name) = &slots[index] else {
                continue;
            };
            let type_oid = prepared
                .types
                .get(index)
                .copied()
                .unwrap_or(types::oid::TEXT);
            let decoded = match value {
                None => tessaridb::Value::Null,
                Some(bytes) if param_formats[index] == 1 => {
                    types::value_of_binary(type_oid, bytes)?
                }
                Some(bytes) => {
                    let text = String::from_utf8(bytes.clone()).map_err(|_| {
                        Refused::surface(code::INVALID_TEXT, "a text parameter is not utf-8")
                    })?;
                    types::value_of_text(type_oid, &text)?
                }
            };
            parameters.insert(name.clone(), decoded);
        }
        Ok(Portal {
            translated: prepared.translated.clone(),
            parameters,
            formats: results.to_vec(),
            spent: None,
            described: false,
            done: false,
            catalog_rows: None,
        })
    }

    async fn describe(
        &mut self,
        conn: &mut Conn,
        held: &mut Held,
        shared: &Arc<super::Shared>,
        kind: u8,
        name: &str,
    ) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        match kind {
            b'S' => self.describe_statement(conn, name).await,
            b'P' => self.describe_portal(conn, held, shared, name).await,
            other => {
                self.fail(
                    conn,
                    &Refused::with_detail(
                        code::PROTOCOL,
                        "Describe names a statement or a portal",
                        format!("kind {other}"),
                    ),
                )
                .await
            }
        }
    }

    /// Describe(statement): the parameter description the Parse owed, then the
    /// projection's shape — or NoData when the projection cannot be named
    /// without running, which a star is.
    async fn describe_statement(&mut self, conn: &mut Conn, name: &str) -> Result<()> {
        let Some(prepared) = self.statements.get(name) else {
            return self
                .fail(
                    conn,
                    &Refused::with_detail(
                        code::UNDEFINED_STATEMENT,
                        "the statement was not prepared on this connection",
                        format!("name {name:?}"),
                    ),
                )
                .await;
        };
        let oids = described_types(prepared);
        conn.send(&codec::write::parameter_description(&oids))
            .await?;
        match columns_of(&prepared.translated) {
            Some(columns) => conn.send(&codec::write::row_description(&columns)).await?,
            None => conn.send(&codec::write::no_data()).await?,
        }
        Ok(())
    }

    /// Describe(portal): the row shape the Execute will answer, so the portal
    /// runs here and an Execute replays — one run per Bind, as the dialect
    /// counts runs.
    async fn describe_portal(
        &mut self,
        conn: &mut Conn,
        held: &mut Held,
        shared: &Arc<super::Shared>,
        name: &str,
    ) -> Result<()> {
        let Some(portal) = self.portals.get_mut(name) else {
            return self
                .fail(
                    conn,
                    &Refused::with_detail(
                        code::UNDEFINED_CURSOR,
                        "the portal is not open on this connection",
                        format!("name {name:?}"),
                    ),
                )
                .await;
        };
        let Some(query) = portal
            .translated
            .statements
            .first()
            .and_then(|shape| shape.catalog.clone())
        else {
            let outcomes = match held
                .run(shared, &portal.translated.script, &portal.parameters)
                .await
            {
                Ok(outcomes) => outcomes,
                Err(failed) => {
                    let refused = run_refusal(failed);
                    return self.fail(conn, &refused).await;
                }
            };
            let mut described = describe_outcome(&outcomes, &portal.translated);
            if let Some(columns) = &mut described
                && let Ok(expanded) = super::expand_formats(&portal.formats, columns.len())
            {
                for (i, column) in columns.iter_mut().enumerate() {
                    column.format = expanded[i];
                }
            }
            portal.spent = Some(outcomes);
            portal.described = true;
            match described {
                Some(columns) => conn.send(&codec::write::row_description(&columns)).await?,
                None => conn.send(&codec::write::no_data()).await?,
            }
            return Ok(());
        };
        // A catalog portal answers locally: evaluate once, the Execute replays.
        let rows = match translate::catalog::evaluate(
            &query,
            &portal.translated.slots,
            &portal.parameters,
        ) {
            Ok(rows) => rows,
            Err(refused) => return self.fail(conn, &refused).await,
        };
        portal.catalog_rows = Some(rows);
        portal.described = true;
        let mut columns = query.columns();
        if let Ok(expanded) = super::expand_formats(&portal.formats, columns.len()) {
            for (i, column) in columns.iter_mut().enumerate() {
                column.format = expanded[i];
            }
        }
        conn.send(&codec::write::row_description(&columns)).await?;
        Ok(())
    }

    async fn execute(
        &mut self,
        conn: &mut Conn,
        held: &mut Held,
        shared: &Arc<super::Shared>,
        name: &str,
        rows: u32,
    ) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        if rows != 0 {
            return self
                .fail(
                    conn,
                    &Refused::surface(
                        code::FEATURE,
                        "row-limited portals are not spoken in this build",
                    ),
                )
                .await;
        }
        let Some(portal) = self.portals.get_mut(name) else {
            return self
                .fail(
                    conn,
                    &Refused::with_detail(
                        code::UNDEFINED_CURSOR,
                        "the portal is not open on this connection",
                        format!("name {name:?}"),
                    ),
                )
                .await;
        };
        if portal.done {
            return conn.send(&codec::write::command_complete("SELECT 0")).await;
        }
        if let Some(query) = portal
            .translated
            .statements
            .first()
            .and_then(|shape| shape.catalog.clone())
        {
            let rows = match portal.catalog_rows.take() {
                Some(rows) => rows,
                None => match translate::catalog::evaluate(
                    &query,
                    &portal.translated.slots,
                    &portal.parameters,
                ) {
                    Ok(rows) => rows,
                    Err(refused) => return self.fail(conn, &refused).await,
                },
            };
            portal.done = true;
            // Execute owes no RowDescription in the extended protocol —
            // Describe answered it, or the client never asked.
            let described = true;
            let formats = portal.formats.clone();
            let answered =
                super::connection::answer_catalog(conn, &query, &rows, described, &formats).await?;
            if !answered {
                self.skipping = true;
                return Ok(());
            }
            return Ok(());
        }
        let outcomes = match portal.spent.take() {
            Some(outcomes) => outcomes,
            None => match held
                .run(shared, &portal.translated.script, &portal.parameters)
                .await
            {
                Ok(outcomes) => outcomes,
                Err(failed) => {
                    let refused = run_refusal(failed);
                    return self.fail(conn, &refused).await;
                }
            },
        };
        portal.done = true;
        // Execute owes no RowDescription in the extended protocol —
        // Describe answered it, or the client never asked.
        let suppress_description = true;
        let shape = match portal.translated.statements.first() {
            Some(shape) => shape,
            None => {
                return self
                    .fail(
                        conn,
                        &Refused::surface(code::INTERNAL, "the portal holds no statement"),
                    )
                    .await;
            }
        };
        let shape = shape.clone();
        let mut described = suppress_description;
        for outcome in &outcomes {
            let answered =
                answer(conn, outcome, &shape, described, &portal.formats.clone()).await?;
            if !answered {
                // An encoding refusal was answered; the Sync to come owns the
                // readiness, and the batch is skipped like any extended error.
                self.skipping = true;
                return Ok(());
            }
            described = false;
        }
        Ok(())
    }

    async fn close(&mut self, conn: &mut Conn, kind: u8, name: &str) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        match kind {
            b'S' => {
                self.statements.remove(name);
            }
            b'P' => {
                self.portals.remove(name);
            }
            other => {
                return self
                    .fail(
                        conn,
                        &Refused::with_detail(
                            code::PROTOCOL,
                            "Close names a statement or a portal",
                            format!("kind {other}"),
                        ),
                    )
                    .await;
            }
        }
        conn.send(&codec::write::close_complete()).await
    }

    /// Sync: the batch ends. The readiness is owed, the skip state clears, and
    /// the unnamed portal closes — exactly one Ready answers one Sync, the
    /// transaction state riding it.
    async fn sync(&mut self, conn: &mut Conn) -> Result<()> {
        self.skipping = false;
        self.portals.remove("");
        conn.send(&codec::write::ready_for_query(self.tx)).await
    }

    /// An extended error: the refusal answers now, the readiness waits for the
    /// Sync, and every message until it is dropped. An error inside a
    /// transaction block aborts it, as the dialect counts.
    async fn fail(&mut self, conn: &mut Conn, refused: &Refused) -> Result<()> {
        conn.refuse_pending(refused).await?;
        self.skipping = true;
        if self.tx == codec::write::Tx::InTx {
            self.tx = codec::write::Tx::Failed;
        }
        Ok(())
    }
}

/// The parameter types Describe(statement) answers: what the Parse declared,
/// the unspecified ones left unspecified (zero) — the port does not invent a
/// type it never resolved, and the dialect's clients read zero as "use your
/// own inference", exactly as the protocol intends.
fn described_types(prepared: &Prepared) -> Vec<u32> {
    let slots = prepared.translated.slots.len();
    let mut oids = Vec::with_capacity(slots);
    for index in 0..slots {
        oids.push(prepared.types.get(index).copied().unwrap_or(0));
    }
    oids
}

/// The columns a projection can name without running: the named ones, or
/// nothing for a star, whose names only the records carry.
fn columns_of(translated: &translate::Translated) -> Option<Vec<codec::write::Column>> {
    let shape = translated.statements.first()?;
    if let Some(query) = &shape.catalog {
        return Some(query.columns());
    }
    match &shape.columns {
        Columns::Named(names) => Some(
            names
                .iter()
                .map(|name| codec::write::Column {
                    name: name.clone(),
                    type_oid: types::oid::TEXT,
                    type_len: -1,
                    format: 0,
                })
                .collect(),
        ),
        Columns::Star => None,
    }
}

/// The row description the first records outcome of a run answers; `None` is
/// NoData — a write's Done, or a star over an empty answer.
fn describe_outcome(
    outcomes: &[Outcome],
    translated: &translate::Translated,
) -> Option<Vec<codec::write::Column>> {
    let shape = translated.statements.first()?;
    match outcomes.first()? {
        Outcome::Records { records, .. } => {
            if records.is_empty() && matches!(shape.columns, Columns::Star) {
                return None;
            }
            let (columns, _) = super::rows(records, &shape.columns, shape.table.as_deref());
            Some(columns)
        }
        Outcome::Value(value) => Some(vec![super::value_column(value)]),
        _ => None,
    }
}

/// A failed run, mapped the way the simple query path maps it.
fn run_refusal(failed: Failed) -> Refused {
    match failed {
        Failed::Refused(refused) => Refused::store(&refused),
        Failed::Carried(carried) => Refused::carried(carried.class, carried.message),
        Failed::Busy => Refused::surface(code::UNAVAILABLE, "the node is busy; try again"),
        Failed::Lost => Refused::surface(code::INTERNAL, "the session is lost"),
    }
}
