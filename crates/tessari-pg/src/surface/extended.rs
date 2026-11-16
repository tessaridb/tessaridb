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
    /// The outcomes a Describe(portal) already produced.
    spent: Option<Vec<Outcome>>,
    /// A Describe already answered this portal's row description.
    described: bool,
    /// An Execute already drained the portal; the dialect answers a spent
    /// portal with an empty completion, never a re-run.
    done: bool,
}

impl Kitchen {
    pub(crate) fn new() -> Self {
        Self {
            statements: HashMap::new(),
            portals: HashMap::new(),
            skipping: false,
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
                    self.statements.insert(
                        name.to_owned(),
                        Prepared {
                            translated,
                            types: types.to_vec(),
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
        if formats.iter().any(|format| *format != 0) {
            return Err(Refused::surface(
                code::FEATURE,
                "binary parameter formats are not spoken in this build",
            ));
        }
        if results.iter().any(|format| *format != 0) {
            return Err(Refused::surface(
                code::FEATURE,
                "binary result formats are not spoken in this build",
            ));
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
            let decoded = match value {
                None => tessaridb::Value::Null,
                Some(bytes) => {
                    let text = String::from_utf8(bytes.clone()).map_err(|_| {
                        Refused::surface(code::INVALID_TEXT, "a text parameter is not utf-8")
                    })?;
                    let type_oid = prepared
                        .types
                        .get(index)
                        .copied()
                        .unwrap_or(types::oid::TEXT);
                    types::value_of_text(type_oid, &text)?
                }
            };
            parameters.insert(name.clone(), decoded);
        }
        Ok(Portal {
            translated: prepared.translated.clone(),
            parameters,
            spent: None,
            described: false,
            done: false,
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
        let described = describe_outcome(&outcomes, &portal.translated);
        portal.spent = Some(outcomes);
        portal.described = true;
        match described {
            Some(columns) => conn.send(&codec::write::row_description(&columns)).await?,
            None => conn.send(&codec::write::no_data()).await?,
        }
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
        let mut described = portal.described;
        for outcome in &outcomes {
            answer(conn, outcome, &portal.translated, described).await?;
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
    /// the unnamed portal closes — exactly one Ready answers one Sync.
    async fn sync(&mut self, conn: &mut Conn) -> Result<()> {
        self.skipping = false;
        self.portals.remove("");
        conn.send(&codec::write::ready_for_query(codec::write::Tx::Idle))
            .await
    }

    /// An extended error: the refusal answers now, the readiness waits for the
    /// Sync, and every message until it is dropped.
    async fn fail(&mut self, conn: &mut Conn, refused: &Refused) -> Result<()> {
        conn.refuse_pending(refused).await?;
        self.skipping = true;
        Ok(())
    }
}

/// The parameter types Describe(statement) answers: what the Parse declared,
/// the unspecified ones read as text, one per placeholder the query used.
fn described_types(prepared: &Prepared) -> Vec<u32> {
    let slots = prepared.translated.slots.len();
    let mut oids = Vec::with_capacity(slots);
    for index in 0..slots {
        oids.push(
            prepared
                .types
                .get(index)
                .copied()
                .unwrap_or(types::oid::TEXT),
        );
    }
    oids
}

/// The columns a projection can name without running: the named ones, or
/// nothing for a star, whose names only the records carry.
fn columns_of(translated: &translate::Translated) -> Option<Vec<codec::write::Column>> {
    match &translated.columns {
        Columns::Named(names) => Some(
            names
                .iter()
                .map(|name| codec::write::Column {
                    name: name.clone(),
                    type_oid: types::oid::TEXT,
                    type_len: -1,
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
    match outcomes.first()? {
        Outcome::Records { records, .. } => {
            if records.is_empty() && matches!(translated.columns, Columns::Star) {
                return None;
            }
            let (columns, _) =
                super::rows(records, &translated.columns, translated.table.as_deref());
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
