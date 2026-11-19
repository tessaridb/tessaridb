//! The surface: a listener on the node's runtime that answers the wire
//! protocol with the node's own tables (ADR-0138).
//!
//! # What a query becomes
//!
//! Every query becomes statements on the connection's own session, with every
//! literal bound and every name checked as an identifier first. A wire client
//! is therefore exactly as able as the store user it signed in as, and a
//! table a script cannot read cannot be read here.
//!
//! # Shape
//!
//! The same as the RESP surface's: bound synchronously so a process learns at
//! start whether it has its address, served on the runtime it is handed, its
//! connections bounded by the same door ceiling and its store calls by the
//! same bridge, and its stages driven by the process through [`Stopping`].

mod answer;
mod connection;
mod copy;
mod errors;
mod extended;
mod running;
mod session;
mod translate;
mod types;

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::codec;
use tessari_constants::{MAX_CONNECTIONS, MAX_STORE_CALLS};
use tessari_serve::{Admitting, Bridge, Stopping};
use tessaridb::Db;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use translate::Columns;

/// The environment variable that names the namespace a connection lands in
/// when the startup names none. Overrides the built-in default; the explicit
/// channels — the `database` field, the `namespace` pair, the `options`
/// string — override it in turn.
pub const NAMESPACE_ENV: &str = "TESSARIDB_PG_NAMESPACE";

/// The namespace a connection lands in when nothing names one: the dialect's
/// own default-schema word, so the mapping reads familiar before it reads
/// documented.
const DEFAULT_NAMESPACE: &str = "public";

/// The default namespace for one value of the override variable: empty or
/// absent names fall back to [`DEFAULT_NAMESPACE`].
pub(crate) fn default_namespace_from(value: Option<String>) -> String {
    value
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_NAMESPACE.to_owned())
}

/// What every connection of one node shares.
pub(crate) struct Shared {
    pub(crate) db: Arc<Db>,
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) namespace_default: String,
    clients: AtomicU64,
}

impl Shared {
    /// The id `BackendKeyData` answers for a new connection.
    pub(crate) fn next_client(&self) -> u64 {
        self.clients
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    /// The cancel key a connection's `BackendKeyData` carries. Cancellation is
    /// not spoken yet, so the key is honest about it: derived from the
    /// connection's id, unguessable enough to keep a stray cancel inert, and
    /// naming nothing that can be stopped.
    pub(crate) fn secret_of(&self, process: u64) -> u32 {
        let mut held = process.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        held ^= held >> 33;
        u32::try_from(held & 0xFFFF_FFFF).unwrap_or(u32::MAX)
    }
}

/// The session facts every connection is greeted with: what this build is,
/// where it landed, and the two settings a client may negotiate at startup.
fn parameters(application: &Option<String>, namespace: &str) -> Vec<(String, String)> {
    let mut out = vec![
        (
            "server_version".to_owned(),
            format!("{VERSION} (tessaridb {})", tessaridb::BUILD_VERSION),
        ),
        ("server_encoding".to_owned(), "UTF8".to_owned()),
        ("client_encoding".to_owned(), "UTF8".to_owned()),
        ("DateStyle".to_owned(), "ISO".to_owned()),
        ("TimeZone".to_owned(), "UTC".to_owned()),
        ("integer_datetimes".to_owned(), "on".to_owned()),
        ("standard_conforming_strings".to_owned(), "on".to_owned()),
        ("default_transaction_read_only".to_owned(), "off".to_owned()),
        ("in_hot_standby".to_owned(), "off".to_owned()),
        ("is_superuser".to_owned(), "on".to_owned()),
        // The namespace the connection landed in, reported under the dialect's
        // own navigation word — SET search_path moves it, SHOW answers it.
        ("search_path".to_owned(), namespace.to_owned()),
    ];
    if let Some(application) = application {
        out.push(("application_name".to_owned(), application.clone()));
    }
    out
}

/// The product version this build answers as: what BI tooling gates on. It is
/// a floor of the dialect this surface speaks, not a claim about the store.
const VERSION: &str = "13.0";

/// Fold a statement's records into the wire's columns and value rows. The
/// column names come from the projection when it named them, and from the
/// first record's fields when it was `*`; the type of a column is its first
/// non-null cell's, and a column of nothing is text. Values stay values here:
/// the wire's format — text or the binary subset — is a send-time choice.
pub(crate) fn rows(
    records: &[(tessaridb::RecordId, tessaridb::Value)],
    projection: &Columns,
    record_names: &[Option<String>],
    table: Option<&str>,
) -> (
    Vec<codec::write::Column>,
    Vec<Vec<Option<tessaridb::Value>>>,
) {
    let names: Vec<String> = match projection {
        // The projection named its columns: the answer owes them in that
        // order, and the records carry their values.
        Columns::Named(names) => names.clone(),
        // A star takes the record's own field order, which is name order.
        Columns::Star => match records.first() {
            Some((_, tessaridb::Value::Object(fields))) => fields.keys().cloned().collect(),
            Some((_, other)) => vec![other.to_string()],
            None => Vec::new(),
        },
    };
    finish(records, names, record_names, table)
}

fn finish(
    records: &[(tessaridb::RecordId, tessaridb::Value)],
    names: Vec<String>,
    record_names: &[Option<String>],
    table: Option<&str>,
) -> (
    Vec<codec::write::Column>,
    Vec<Vec<Option<tessaridb::Value>>>,
) {
    let mut columns: Vec<codec::write::Column> = names
        .iter()
        .map(|name| codec::write::Column {
            name: name.clone(),
            type_oid: types::oid::TEXT,
            type_len: -1,
            format: 0,
        })
        .collect();
    let mut rows = Vec::with_capacity(records.len());
    for (identity, record) in records {
        let mut row = Vec::with_capacity(columns.len());
        for (i, column) in columns.iter_mut().enumerate() {
            // The record's identity answers a projection of `id`: the store
            // keeps it beside the record, and this is the one name under
            // which the dialect can ask for it.
            let value = match record {
                tessaridb::Value::Object(fields) => {
                    let key = record_names
                        .get(i)
                        .and_then(|held| held.as_ref())
                        .unwrap_or(&names[i]);
                    fields.get(key).cloned().unwrap_or_else(|| {
                        if names[i] == "id" {
                            // The store holds the identity beside the record;
                            // the dialect reads it as the table and the key,
                            // spelled the way the store spells an address.
                            tessaridb::Value::String(match table {
                                Some(table) => format!("{table}:{identity}"),
                                None => identity.to_string(),
                            })
                        } else {
                            tessaridb::Value::None
                        }
                    })
                }
                single => single.clone(),
            };
            if !matches!(value, tessaridb::Value::None | tessaridb::Value::Null)
                && column.type_oid == types::oid::TEXT
            {
                column.type_oid = types::type_of(&value);
                column.type_len = types::size_of(column.type_oid);
            }
            row.push(Some(value));
        }
        rows.push(row);
    }
    (columns, rows)
}

/// The format word per column, from what a Bind asked: one word names every
/// column, a word per column names each; nothing asks for text everywhere.
pub(crate) fn expand_formats(
    formats: &[u16],
    columns: usize,
) -> std::result::Result<Vec<u16>, errors::Refused> {
    match formats.len() {
        0 => Ok(vec![0; columns]),
        1 => Ok(vec![formats[0]; columns]),
        n if n == columns => Ok(formats.to_vec()),
        n => Err(errors::Refused::with_detail(
            errors::code::PROTOCOL,
            "the result formats do not match the answer's columns",
            format!("{n} formats for {columns} columns"),
        )),
    }
}

/// Encode value rows under their columns and the formats a Bind asked for:
/// text everywhere unless a column asked for the binary subset.
pub(crate) fn encode(
    columns: &[codec::write::Column],
    rows: &[Vec<Option<tessaridb::Value>>],
    formats: &[u16],
) -> std::result::Result<Vec<Vec<Option<Vec<u8>>>>, errors::Refused> {
    let expanded = expand_formats(formats, columns.len())?;
    for format in &expanded {
        if *format > 1 {
            return Err(errors::Refused::surface(
                errors::code::PROTOCOL,
                "a result format other than text or binary is not spoken",
            ));
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(row.len());
        for (i, value) in row.iter().enumerate() {
            let held = value.as_ref().unwrap_or(&tessaridb::Value::None);
            let cell = if expanded[i] == 1 {
                types::binary_of(held, columns[i].type_oid)
            } else {
                Ok(types::text_of(held).map(String::into_bytes))
            }?;
            cells.push(cell);
        }
        out.push(cells);
    }
    Ok(out)
}

/// One unnamed value's column, as `SELECT 1` answers it.
pub(crate) fn value_column(value: &tessaridb::Value) -> codec::write::Column {
    let type_oid = match value {
        tessaridb::Value::None | tessaridb::Value::Null => types::oid::TEXT,
        held => types::type_of(held),
    };
    codec::write::Column {
        name: translate::UNNAMED.to_owned(),
        type_oid,
        type_len: types::size_of(type_oid),
        format: 0,
    }
}

/// A bound surface, not yet serving. The node's own state stays plain until
/// [`Node::serve`] assembles the connection-shared [`Shared`]: before that
/// the builder is free to move things.
pub struct Node {
    listener: std::net::TcpListener,
    db: Arc<Db>,
    bridge: Arc<Bridge>,
    stopping: Arc<Stopping>,
    namespace_default: String,
    door: Arc<Admitting>,
    secured: Option<TlsAcceptor>,
}

impl Node {
    /// Bind `address` for the wire protocol over `db`. The default namespace —
    /// where a connection lands when its startup names none — comes from
    /// [`NAMESPACE_ENV`] when the variable holds a name, else from
    /// `public`.
    ///
    /// # Errors
    ///
    /// The operating system's failure when the address cannot be bound.
    pub fn bind(db: Arc<Db>, address: impl ToSocketAddrs) -> std::io::Result<Self> {
        let listener = tessari_serve::listen(address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            db,
            bridge: Arc::new(Bridge::new(MAX_STORE_CALLS)),
            stopping: Stopping::new(),
            namespace_default: default_namespace_from(
                std::env::var(NAMESPACE_ENV).ok().filter(|v| !v.is_empty()),
            ),
            door: Admitting::to(MAX_CONNECTIONS),
            secured: None,
        })
    }

    /// Name the namespace a connection lands in when its startup names none.
    /// Overrides [`NAMESPACE_ENV`] for this node; the explicit startup
    /// channels override it in turn.
    #[must_use]
    pub fn defaulting_namespace(mut self, namespace: impl Into<String>) -> Self {
        let namespace = namespace.into();
        let name = namespace.trim();
        if !name.is_empty() {
            self.namespace_default = name.to_owned();
        }
        self
    }

    /// Answer `S` to an SSLRequest and wrap what follows in TLS. Without a
    /// credential the surface answers `N` and speaks in the clear, and the
    /// start banner says so, as the node's other surfaces do.
    #[must_use]
    pub fn securing(mut self, settings: Arc<rustls::ServerConfig>) -> Self {
        self.secured = Some(TlsAcceptor::from(settings));
        self
    }

    /// Whether TLS will wrap the connections that ask for it.
    #[must_use]
    pub fn is_secured(&self) -> bool {
        self.secured.is_some()
    }

    /// Where it is listening.
    ///
    /// # Errors
    ///
    /// The operating system's failure when the socket cannot say.
    pub fn address(&self) -> std::io::Result<String> {
        Ok(self.listener.local_addr()?.to_string())
    }

    /// What this surface counts as in flight, and how it is told to stop;
    /// taken before [`Node::serve`].
    #[must_use]
    pub fn stopping(&self) -> Arc<Stopping> {
        Arc::clone(&self.stopping)
    }

    /// Accept connections until `stop` is cancelled. Must be awaited inside a
    /// Tokio runtime; the surface creates none.
    ///
    /// Only the accepting stops: a query in flight finishes, which is what the
    /// drain waits on through [`Stopping`].
    ///
    /// # Errors
    ///
    /// The listener's failure when accepting fails in a way that does not pass.
    pub async fn serve(&self, stop: CancellationToken) -> std::io::Result<()> {
        let shared = Arc::new(Shared {
            db: Arc::clone(&self.db),
            bridge: Arc::clone(&self.bridge),
            namespace_default: self.namespace_default.clone(),
            clients: AtomicU64::new(0),
        });
        let listener = tokio::net::TcpListener::from_std(self.listener.try_clone()?)?;
        let mut connections = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(why) = joined
                        && why.is_panic()
                    {
                        tracing::warn!(error = %why, "a wire connection ended in a panic");
                    }
                    continue;
                }
                accepted = listener.accept() => accepted,
            };
            if self.stopping.asked() {
                break;
            }
            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(why) if tessari_serve::passes(&why) => continue,
                Err(why) => return Err(why),
            };
            let Some(place) = self.door.admit() else {
                tracing::warn!(from = %peer, open = self.door.limit(), "wire connection refused: as many open as this node takes");
                continue;
            };
            let shared = Arc::clone(&shared);
            let secured = self.secured.clone();
            let stop = stop.clone();
            connections.spawn(
                async move {
                    let _place = place;
                    connection::serve(stream, peer, shared, secured, stop).await;
                }
                .instrument(tracing::info_span!("pg", %peer)),
            );
        }
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}

#[cfg(test)]
mod tests;
