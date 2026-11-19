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

use super::answer::answer;
use super::connection::Conn;
use super::errors::{Refused, code};
use super::running::{Held, run_refusal};
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
    /// The session parameters this connection answers SET/SHOW/RESET
    /// against — seeded from the startup greeting's own reported set, so a
    /// SHOW never answers a value the client never saw reported. One
    /// connection is one task, so a plain map is the right hold.
    pub(crate) params: std::collections::BTreeMap<String, String>,
    /// The startup-reported values RESET returns to.
    pub(crate) defaults: std::collections::BTreeMap<String, String>,
}

/// What a Parse named: a read or write, or a transaction verb the Execute
/// drives like the simple path's own block words.
enum Prepared {
    /// A read or write: its translation and the parameter types it declared.
    Query {
        translated: translate::Translated,
        types: Vec<u32>,
    },
    /// `BEGIN` / `COMMIT` / `ROLLBACK` parsed on the extended path.
    Verb(super::session::TxVerb),
}

/// A bound portal: the statement's shape, the values its placeholders took,
/// and what a Describe already ran so an Execute replays instead of re-runs.
struct Portal {
    /// A transaction verb this portal drives at Execute, where the statement
    /// held no translation at all.
    verb: Option<super::session::TxVerb>,
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
    /// How many rows a row-limited Execute already answered, so the next
    /// Execute resumes where it left (PortalSuspended).
    served: usize,
}

impl Kitchen {
    pub(crate) fn new() -> Self {
        Self {
            statements: HashMap::new(),
            portals: HashMap::new(),
            skipping: false,
            tx: codec::write::Tx::Idle,
            params: std::collections::BTreeMap::new(),
            defaults: std::collections::BTreeMap::new(),
        }
    }

    /// Seed the parameter hold from the greeting's own reported set — one
    /// source of truth, so SHOW never answers a value ParameterStatus never
    /// carried. `application_name` is settable whether or not the startup
    /// named one; its default is the empty name.
    pub(crate) fn seed_params(&mut self, reported: &[(String, String)]) {
        for (name, value) in reported {
            self.params.insert(name.clone(), value.clone());
            self.defaults.insert(name.clone(), value.clone());
        }
        self.params
            .entry("application_name".to_owned())
            .or_default();
        self.defaults
            .entry("application_name".to_owned())
            .or_default();
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
            Frontend::Parse { name, query, types } => {
                self.parse(conn, held, shared, &name, &query, &types).await
            }
            Frontend::Bind {
                portal,
                statement,
                formats,
                values,
                results,
            } => {
                if self.skipping {
                    return Ok(());
                }
                match self.prepare_bind(&portal, &statement, &formats, &values, &results) {
                    Ok(bound) => {
                        self.portals.insert(portal, bound);
                        conn.send(&codec::write::bind_complete()).await?;
                    }
                    // A Bind refusal is an error like any other while a block
                    // is open: it aborts, and the engine-side block drops with
                    // the state the readiness reports.
                    Err(refused) => self.fail(conn, held, shared, &refused).await?,
                }
                Ok(())
            }
            Frontend::Describe { kind, name } => {
                self.describe(conn, held, shared, kind, &name).await
            }
            Frontend::Execute { portal, rows } => {
                self.execute(conn, held, shared, &portal, rows).await
            }
            Frontend::Close { kind, name } => self.close(conn, held, shared, kind, &name).await,
            Frontend::Sync => self.sync(conn, held).await,
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
        held: &mut Held,
        shared: &Arc<super::Shared>,
        name: &str,
        query: &str,
        types: &[u32],
    ) -> Result<()> {
        if self.skipping {
            return Ok(());
        }
        // The session commands parse first: a transaction verb rides the
        // extended protocol like any statement (the dialect's drivers send
        // verbs this way); the other session commands speak on the simple
        // query path and refuse by name here.
        let refusal = match super::session::parse(query) {
            Ok(Some(super::session::Session::Transaction(verb))) => {
                if !types.is_empty() {
                    Some(Refused::surface(
                        code::FEATURE,
                        "a transaction verb takes no parameters in this build",
                    ))
                } else if name.is_empty() || self.statements.len() < MAX_NAMED {
                    self.statements
                        .insert(name.to_owned(), Prepared::Verb(verb));
                    conn.send(&codec::write::parse_complete()).await?;
                    None
                } else {
                    Some(Refused::surface(
                        code::LIMIT,
                        "this connection holds as many named statements as it takes",
                    ))
                }
            }
            Ok(Some(_)) => Some(Refused::surface(
                code::FEATURE,
                "this session command speaks on the simple query path in this build",
            )),
            Ok(None) => match translate::translate(query) {
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
                        // Store-side inference: a `column = $n` the Parse left
                        // unspecified reads the column's type from the table's
                        // own catalog, as the dialect's servers infer at Parse
                        // time. A table the store refuses refuses the Parse in
                        // the store's words, exactly where a real server fails.
                        let unresolved =
                            |index: usize| resolved.get(index).copied().unwrap_or(0) == 0;
                        let first = translated.statements.first();
                        let needs_inference = first.is_some_and(|shape| {
                            shape.catalog.is_none()
                                && shape.inference.iter().any(|(slot, _)| unresolved(*slot))
                        });
                        let inference = if needs_inference {
                            let Some(shape) = first else {
                                unreachable!("the check above names it");
                            };
                            infer_table_types(
                                held,
                                shared,
                                &shape.inference,
                                shape.table.as_deref(),
                                &resolved,
                            )
                            .await
                            .map(|filled| {
                                for (index, oid) in filled {
                                    resolved.resize(index.saturating_add(1).max(resolved.len()), 0);
                                    if resolved[index] == 0 {
                                        resolved[index] = oid;
                                    }
                                }
                            })
                        } else {
                            Ok(())
                        };
                        match inference {
                            Err(refused) => Some(refused),
                            Ok(()) => {
                                self.statements.insert(
                                    name.to_owned(),
                                    Prepared::Query {
                                        translated,
                                        types: resolved,
                                    },
                                );
                                conn.send(&codec::write::parse_complete()).await?;
                                None
                            }
                        }
                    } else {
                        Some(Refused::surface(
                            code::LIMIT,
                            "this connection holds as many named statements as it takes",
                        ))
                    }
                }
            },
            Err(refused) => Some(refused),
        };
        if let Some(refused) = refusal {
            self.fail(conn, held, shared, &refused).await?;
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
        let (slot_count, verb) = match prepared {
            Prepared::Query { translated, .. } => (translated.slots.len(), None),
            Prepared::Verb(verb) => (0, Some(*verb)),
        };
        if values.len() != slot_count {
            return Err(Refused::with_detail(
                code::PROTOCOL,
                "the Bind's values do not match the statement's parameters",
                format!("expected {}, got {}", slot_count, values.len()),
            ));
        }
        if !portal.is_empty() && self.portals.len() >= MAX_NAMED {
            return Err(Refused::surface(
                code::LIMIT,
                "this connection holds as many named portals as it takes",
            ));
        }
        let Prepared::Query { translated, types } = prepared else {
            return Ok(Portal {
                verb,
                translated: translate::Translated {
                    script: String::new(),
                    parameters: tessaridb::Parameters::new(),
                    statements: Vec::new(),
                    slots: Vec::new(),
                },
                parameters: tessaridb::Parameters::new(),
                formats: results.to_vec(),
                spent: None,
                described: false,
                done: false,
                catalog_rows: None,
                served: 0,
            });
        };
        let slots = &translated.slots;
        let mut parameters = translated.parameters.clone();
        for (index, value) in values.iter().enumerate() {
            let Some(name) = &slots[index] else {
                continue;
            };
            let type_oid = types.get(index).copied().unwrap_or(types::oid::TEXT);
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
            verb,
            translated: translated.clone(),
            parameters,
            formats: results.to_vec(),
            spent: None,
            described: false,
            done: false,
            catalog_rows: None,
            served: 0,
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
            b'S' => self.describe_statement(conn, held, shared, name).await,
            b'P' => self.describe_portal(conn, held, shared, name).await,
            other => {
                self.fail(
                    conn,
                    held,
                    shared,
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

    /// Describe(statement): the parameter description the Parse owed, then
    /// the projection's shape — or NoData when the projection cannot be
    /// named without running, which a star is. A plain column projection
    /// reads the column types from the table's own catalog, as the
    /// dialect's servers answer at statement time; a table the catalog does
    /// not know leaves the columns text, and the Execute refuses in the
    /// store's words where the read actually runs.
    async fn describe_statement(
        &mut self,
        conn: &mut Conn,
        held: &mut Held,
        shared: &Arc<super::Shared>,
        name: &str,
    ) -> Result<()> {
        let Some(prepared) = self.statements.get(name) else {
            return self
                .fail(
                    conn,
                    held,
                    shared,
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
        if matches!(prepared, Prepared::Verb(_)) {
            conn.send(&codec::write::no_data()).await?;
            return Ok(());
        }
        let Prepared::Query { translated, .. } = prepared else {
            unreachable!("the verb arm above returns");
        };
        let mut columns = columns_of(translated);
        if let Some(columns) = &mut columns
            && let Some(shape) = translated.statements.first()
            && let Some(table) = shape.table.as_deref()
            && let Ok(fields) = table_fields(held, shared, table).await
        {
            for column in columns.iter_mut() {
                if column.type_oid == types::oid::TEXT
                    && let Some(oid) = fields.get(&column.name)
                {
                    column.type_oid = *oid;
                    column.type_len = types::size_of(*oid);
                }
            }
        }
        match columns {
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
                    held,
                    shared,
                    &Refused::with_detail(
                        code::UNDEFINED_CURSOR,
                        "the portal is not open on this connection",
                        format!("name {name:?}"),
                    ),
                )
                .await;
        };
        if portal.verb.is_some() {
            conn.send(&codec::write::no_data()).await?;
            return Ok(());
        }
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
                    return self.fail(conn, held, shared, &refused).await;
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
        let dynamic = match catalog_dynamic(
            held,
            shared,
            &query,
            &portal.translated.slots,
            &portal.parameters,
        )
        .await
        {
            Ok(dynamic) => dynamic,
            Err(refused) => return self.fail(conn, held, shared, &refused).await,
        };
        let rows = match translate::catalog::evaluate(
            &query,
            &portal.translated.slots,
            &portal.parameters,
            dynamic.as_ref(),
            held.user(),
        ) {
            Ok(rows) => rows,
            Err(refused) => return self.fail(conn, held, shared, &refused).await,
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
        let Some(portal) = self.portals.get_mut(name) else {
            return self
                .fail(
                    conn,
                    held,
                    shared,
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
        if let Some(verb) = portal.verb {
            // The verb the Parse named runs here, against the connection's
            // own block state — the same words the simple path answers.
            portal.done = true;
            return super::answer::transaction(conn, verb, &mut self.tx, held, shared, false).await;
        }
        if let Some(query) = portal
            .translated
            .statements
            .first()
            .and_then(|shape| shape.catalog.clone())
        {
            let catalog_rows = match portal.catalog_rows.take() {
                Some(rows) => rows,
                None => {
                    let dynamic = match catalog_dynamic(
                        held,
                        shared,
                        &query,
                        &portal.translated.slots,
                        &portal.parameters,
                    )
                    .await
                    {
                        Ok(dynamic) => dynamic,
                        Err(refused) => return self.fail(conn, held, shared, &refused).await,
                    };
                    match translate::catalog::evaluate(
                        &query,
                        &portal.translated.slots,
                        &portal.parameters,
                        dynamic.as_ref(),
                        held.user(),
                    ) {
                        Ok(rows) => rows,
                        Err(refused) => return self.fail(conn, held, shared, &refused).await,
                    }
                }
            };
            let formats = portal.formats.clone();
            if rows != 0 {
                // A row ceiling: the window answers, the rest holds.
                let start = portal.served.min(catalog_rows.len());
                let end = start
                    .saturating_add(usize::try_from(rows).unwrap_or(usize::MAX))
                    .min(catalog_rows.len());
                portal.served = end;
                let window = &catalog_rows[start..end];
                let described = true;
                let answered =
                    super::answer::answer_catalog(conn, &query, window, described, &formats)
                        .await?;
                if !answered {
                    self.skipping = true;
                    return Ok(());
                }
                if end < catalog_rows.len() {
                    return conn.send(&codec::write::portal_suspended()).await;
                }
                portal.done = true;
                return Ok(());
            }
            portal.done = true;
            // Execute owes no RowDescription in the extended protocol —
            // Describe answered it, or the client never asked.
            let described = true;
            let answered =
                super::answer::answer_catalog(conn, &query, &catalog_rows, described, &formats)
                    .await?;
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
                    return self.fail(conn, held, shared, &refused).await;
                }
            },
        };
        let shape = match portal.translated.statements.first() {
            Some(shape) => shape,
            None => {
                return self
                    .fail(
                        conn,
                        held,
                        shared,
                        &Refused::surface(code::INTERNAL, "the portal holds no statement"),
                    )
                    .await;
            }
        };
        let shape = shape.clone();
        if rows != 0 {
            // A row ceiling over the first records outcome: the window
            // answers as rows, the rest holds for the next Execute — the
            // count the completion names is the portal's, as the protocol
            // counts runs.
            if let Some(tessaridb::Outcome::Records { records, .. }) = outcomes.first() {
                let (columns, all) = super::rows(
                    records,
                    &shape.columns,
                    &shape.record_names,
                    shape.table.as_deref(),
                );
                // The OFFSET head drops once, at the head: the windows page
                // what remains — the count the completion names rides the
                // post-offset rows, never the dropped front.
                let head = usize::try_from(shape.offset)
                    .unwrap_or(usize::MAX)
                    .min(all.len());
                let page = &all[head..];
                let start = portal.served.min(page.len());
                let end = start
                    .saturating_add(usize::try_from(rows).unwrap_or(usize::MAX))
                    .min(page.len());
                portal.served = end;
                let window = &page[start..end];
                let encoded = match super::encode(&columns, window, &portal.formats) {
                    Ok(encoded) => encoded,
                    Err(refused) => {
                        conn.refuse_pending(&refused).await?;
                        self.skipping = true;
                        return Ok(());
                    }
                };
                for row in &encoded {
                    conn.send(&codec::write::data_row(row)).await?;
                }
                if end < page.len() {
                    portal.spent = Some(outcomes);
                    return conn.send(&codec::write::portal_suspended()).await;
                }
                portal.done = true;
                return conn
                    .send(&codec::write::command_complete(&shape.tag.completion(end)))
                    .await;
            }
        }
        portal.done = true;
        // Execute owes no RowDescription in the extended protocol —
        // Describe answered it, or the client never asked.
        let suppress_description = true;
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

    async fn close(
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
                        held,
                        shared,
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
    /// transaction state riding it. The state is reconciled with what the
    /// session actually holds: a portal run may have closed the block (a
    /// string carrying its own `COMMIT`) or none on this connection may have
    /// opened one yet, and the readiness answers the engine, not the hope.
    async fn sync(&mut self, conn: &mut Conn, held: &Held) -> Result<()> {
        self.skipping = false;
        self.portals.remove("");
        self.tx = match (self.tx, held.transaction_open()) {
            (codec::write::Tx::InTx, false) => codec::write::Tx::Idle,
            (codec::write::Tx::Idle, true) => codec::write::Tx::InTx,
            (state, _) => state,
        };
        conn.send(&codec::write::ready_for_query(self.tx)).await
    }

    /// An extended error: the refusal answers now, the readiness waits for the
    /// Sync, and every message until it is dropped. An error inside a
    /// transaction block aborts it, as the dialect counts — the block the
    /// session parked is dropped engine-side too, so nothing stays held
    /// behind a Failed readiness.
    async fn fail(
        &mut self,
        conn: &mut Conn,
        held: &mut Held,
        shared: &Arc<super::Shared>,
        refused: &Refused,
    ) -> Result<()> {
        conn.refuse_pending(refused).await?;
        self.skipping = true;
        if self.tx == codec::write::Tx::InTx {
            self.tx = codec::write::Tx::Failed;
            // The discard's own refusal, should one arrive, changes nothing
            // the client has not already been told.
            let _discard = held
                .run(shared, "CANCEL", &tessaridb::Parameters::new())
                .await;
        }
        Ok(())
    }
}

/// The parameter types Describe(statement) answers: what the Parse declared
/// or inferred, the unspecified ones left unspecified (zero) — the port does
/// not invent a type it never resolved, and the dialect's clients read zero
/// as "use your own inference", exactly as the protocol intends.
fn described_types(prepared: &Prepared) -> Vec<u32> {
    let Prepared::Query { translated, types } = prepared else {
        return Vec::new();
    };
    let slots = translated.slots.len();
    let mut oids = Vec::with_capacity(slots);
    for index in 0..slots {
        oids.push(types.get(index).copied().unwrap_or(0));
    }
    oids
}

/// Parse-time type inference: the statement's `column = $n` comparisons name
/// the columns the unspecified parameters ride on; the table's own catalog
/// answer (`INFO FOR TABLE`) names each column's kind, which the one
/// kind→oid map turns into the wire type. An explicit Parse oid always wins;
/// a column the table does not declare stays unspecified, as before.
async fn infer_table_types(
    held: &mut Held,
    shared: &Arc<super::Shared>,
    inference: &[(usize, String)],
    table: Option<&str>,
    resolved: &[u32],
) -> std::result::Result<Vec<(usize, u32)>, Refused> {
    let Some(table) = table else {
        return Err(Refused::surface(
            code::INTERNAL,
            "the inference lost its table",
        ));
    };
    let fields = table_fields(held, shared, table).await?;
    let mut filled = Vec::new();
    for (slot, column) in inference {
        if resolved.get(*slot).copied().unwrap_or(0) != 0 {
            continue;
        }
        let key = column.rsplit('.').next().unwrap_or(column);
        if let Some(oid) = fields.get(key) {
            filled.push((*slot, *oid));
        }
    }
    Ok(filled)
}

/// The table's declared fields under their wire types — the one catalog
/// read both inferences ride on.
async fn table_fields(
    held: &mut Held,
    shared: &Arc<super::Shared>,
    table: &str,
) -> std::result::Result<std::collections::BTreeMap<String, u32>, Refused> {
    let outcomes = held
        .run(
            shared,
            &format!("INFO FOR TABLE {table};"),
            &Parameters::new(),
        )
        .await
        .map_err(run_refusal)?;
    let Some(Outcome::Value(tessaridb::Value::Object(report))) = outcomes.first() else {
        return Err(Refused::surface(
            code::INTERNAL,
            "the store's table report did not read",
        ));
    };
    Ok(match report.get("fields") {
        Some(tessaridb::Value::Array(items)) => items
            .iter()
            .filter_map(|field| match field {
                tessaridb::Value::Object(described) => {
                    let name = match described.get("name") {
                        Some(tessaridb::Value::String(name)) => name.clone(),
                        _ => return None,
                    };
                    let kind = match described.get("type") {
                        Some(tessaridb::Value::String(kind)) => kind.clone(),
                        _ => String::new(),
                    };
                    Some((name, translate::catalog::dynamic::kind_oid(&kind)))
                }
                _ => None,
            })
            .collect(),
        _ => std::collections::BTreeMap::new(),
    })
}

/// The user-table rows a catalog query names, introspected once per answer —
/// `None` when the query reads the static relations alone.
async fn catalog_dynamic(
    held: &mut Held,
    shared: &Arc<super::Shared>,
    query: &translate::catalog::Query,
    slots: &[Option<String>],
    parameters: &Parameters,
) -> std::result::Result<Option<translate::catalog::Dynamic>, Refused> {
    if !translate::catalog::dynamic::needed(std::slice::from_ref(&query)) {
        return Ok(None);
    }
    translate::catalog::dynamic::load(
        held,
        shared,
        translate::catalog::dynamic::wants_attributes(std::slice::from_ref(&query)),
        translate::catalog::dynamic::wants_viewdefs(std::slice::from_ref(&query)),
        query.pinned_oid(slots, parameters),
    )
    .await
}

/// The columns a projection can name without running: the named ones, or
/// nothing for a star, whose names only the records carry.
fn columns_of(translated: &translate::Translated) -> Option<Vec<codec::write::Column>> {
    let shape = translated.statements.first()?;
    if let Some(query) = &shape.catalog {
        return Some(query.columns());
    }
    if shape.tag != translate::write::Tag::Select && shape.returning.is_none() {
        // A write the client did not RETURN owes NoData: no row shape the
        // Execute could describe.
        return None;
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
            let (columns, _) = super::rows(
                records,
                &shape.columns,
                &shape.record_names,
                shape.table.as_deref(),
            );
            Some(columns)
        }
        Outcome::Value(value) => Some(vec![super::value_column(value)]),
        _ => None,
    }
}
