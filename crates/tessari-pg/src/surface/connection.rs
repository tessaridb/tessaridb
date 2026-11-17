//! One connection: the startup negotiation, the authentication cycle, and
//! the query loop, over a stream that may be TLS or plain per the client's
//! own SSLRequest.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;

use crate::codec::{self, First, Frontend};
use crate::error::{Error, Result};

use super::Shared;
use super::errors::{Refused, code};
use super::running::{Failed, Held};
use super::translate;
use super::types;

/// Serve one accepted connection until it ends, the node stops, or the
/// stream fails. Panics stay inside the task; the JoinSet logs them.
pub(crate) async fn serve(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    shared: Arc<Shared>,
    secured: Option<TlsAcceptor>,
    stop: CancellationToken,
) {
    let (mut conn, buffered) = match Conn::accept(stream, secured).await {
        Ok(accepted) => accepted,
        Err(why) => {
            tracing::debug!(%peer, error = %why, "the startup negotiation did not begin");
            return;
        }
    };
    if let Err(why) = session(&mut conn, buffered, &shared, stop).await {
        tracing::debug!(%peer, error = %why, "the connection ended");
    }
}

/// The startup, the authentication, and then queries until the client goes
/// away or the node stops. `buffered` is the first packet when the
/// negotiation already read it (a startup with no SSLRequest before it).
async fn session(
    conn: &mut Conn,
    buffered: Option<First>,
    shared: &Arc<Shared>,
    stop: CancellationToken,
) -> Result<()> {
    let first = match buffered {
        Some(first) => first,
        None => conn.first().await?,
    };
    let First::Startup { version, pairs } = first else {
        match first {
            // A second negotiation inside the negotiated channel is refused,
            // as the protocol's own rule for this case says.
            First::Ssl => {
                conn.refuse_startup(&Refused::surface(
                    code::FEATURE,
                    "TLS is already negotiated on this connection",
                ))
                .await?;
            }
            // A cancel request names no work this build can stop; the
            // protocol answers it by closing without a word.
            First::Cancel { .. } => {}
            First::Startup { .. } => unreachable!("matched above"),
        }
        return Ok(());
    };
    if version >> 16 != codec::PROTOCOL_VERSION >> 16 {
        conn.refuse_startup(&Refused::with_detail(
            code::FEATURE,
            "this build speaks protocol 3.0 of the dialect",
            format!("offered {:#x}", version),
        ))
        .await?;
        return Ok(());
    }
    let user = pairs
        .iter()
        .find(|(key, _)| key == "user")
        .map(|(_, value)| value.clone());
    let database = pairs
        .iter()
        .find(|(key, _)| key == "database")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    // The tenancy the connection lands in, from the most specific channel
    // that named one: the database field's <namespace>.<database>, then the
    // namespace startup pair, then the options string's -c namespace=, then
    // the node's default. A bare database name reads inside the default
    // namespace; no name at all follows the dialect's own convention — the
    // database defaults to the user's name.
    let pair_namespace = pairs
        .iter()
        .find(|(key, _)| key == "namespace")
        .map(|(_, value)| value.clone());
    let options_namespace = pairs
        .iter()
        .find(|(key, _)| key == "options")
        .and_then(|(_, options)| option_namespace(options));
    let (namespace, chosen) = match database.split_once('.') {
        Some((namespace, chosen)) => {
            if namespace.is_empty() || chosen.is_empty() || chosen.contains('.') {
                conn.refuse_startup(&Refused::with_detail(
                    code::INVALID_CATALOG,
                    "the startup database names exactly one namespace and one database",
                    format!("got {database:?}"),
                ))
                .await?;
                return Ok(());
            }
            (namespace.to_owned(), chosen.to_owned())
        }
        None => {
            if database.contains('.') {
                conn.refuse_startup(&Refused::with_detail(
                    code::INVALID_CATALOG,
                    "the startup database names exactly one namespace and one database",
                    format!("got {database:?}"),
                ))
                .await?;
                return Ok(());
            }
            let namespace = pair_namespace
                .or(options_namespace)
                .unwrap_or_else(|| shared.namespace_default.clone());
            let chosen = if database.is_empty() {
                user.clone().filter(|name| !name.is_empty())
            } else {
                Some(database.clone())
            }
            .unwrap_or_else(|| "main".to_owned());
            (namespace, chosen)
        }
    };
    let application = pairs
        .iter()
        .find(|(key, _)| key == "application_name")
        .map(|(_, value)| value.clone());

    let mut held = Held::anonymous(shared, &namespace, &chosen);
    if let Some(user) = user {
        conn.send(&codec::write::cleartext_password()).await?;
        let Frontend::Password(password) = conn.message().await? else {
            conn.refuse_startup(&Refused::surface(
                code::INVALID_PASSWORD,
                "a password was expected answering the challenge",
            ))
            .await?;
            return Ok(());
        };
        match held.sign_in(shared, &user, &password).await {
            Ok(()) => {}
            Err(Failed::Refused(refused)) => {
                let refused = Refused::store(&refused);
                conn.refuse_startup(&refused).await?;
                return Ok(());
            }
            Err(Failed::Busy) => {
                conn.refuse_startup(&Refused::surface(
                    code::UNAVAILABLE,
                    "the node is busy; try again",
                ))
                .await?;
                return Ok(());
            }
            Err(Failed::Carried(_) | Failed::Lost) => {
                conn.refuse_startup(&Refused::surface(
                    code::INTERNAL,
                    "the sign-in did not complete",
                ))
                .await?;
                return Ok(());
            }
        }
    }

    conn.send(&codec::write::authentication_ok()).await?;
    for (name, value) in super::parameters(&application, held.namespace()) {
        conn.send(&codec::write::parameter_status(&name, &value))
            .await?;
    }
    let process = shared.next_client();
    conn.send(&codec::write::backend_key_data(
        u32::try_from(process).unwrap_or(u32::MAX),
        shared.secret_of(process),
    ))
    .await?;
    conn.send(&codec::write::ready_for_query(codec::write::Tx::Idle))
        .await?;

    // The kitchen carries the transaction state the readiness reports. The
    // store's own transactions close inside the script that opened them, so
    // a wire-level block is the connection's own tracking: the statements
    // inside answer as themselves, and the divergence a held snapshot would
    // close is named on the defer list.
    let mut kitchen = super::extended::Kitchen::new();
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return Ok(()),
            message = conn.message() => {
                match message? {
                    Frontend::Terminate => return Ok(()),
                    Frontend::Query(query) => {
                        // A simple Query is a synchronisation point: it ends
                        // any extended skip state, then answers as simple
                        // queries answer.
                        kitchen.interrupt();
                        query_once(conn, &query, &mut held, shared, &mut kitchen).await?;
                    }
                    extended @ (Frontend::Parse { .. }
                    | Frontend::Bind { .. }
                    | Frontend::Describe { .. }
                    | Frontend::Execute { .. }
                    | Frontend::Close { .. }
                    | Frontend::Sync
                    | Frontend::Flush) => {
                        kitchen.handle(conn, extended, &mut held, shared).await?;
                    }
                    Frontend::Password(_) => {
                        conn.refuse(
                            &Refused::surface(
                                code::INVALID_AUTHORIZATION,
                                "the session is already signed in",
                            ),
                            kitchen.tx,
                        )
                        .await?;
                    }
                    Frontend::Unspoken(kind) => {
                        // A message this build does not speak at all: the
                        // error answers it, and the Sync the client sends next
                        // carries the readiness — one Ready, as the protocol
                        // counts them.
                        conn.refuse_pending(&Refused::with_detail(
                            code::FEATURE,
                            "this message is not spoken in this build",
                            format!("message kind {kind}"),
                        ))
                        .await?;
                    }
                }
            }
        }
    }
}

/// One simple query: the session commands answer first — `SET search_path`
/// moves the connection, `SHOW search_path` answers where it reads, and the
/// transaction verbs walk the connection's block state. Then the usual path:
/// blank answers empty; the translation refuses by name; a store refusal
/// carries the store's words; rows come back described. A refusal inside a
/// block aborts it, as the dialect's own state machine counts.
async fn query_once(
    conn: &mut Conn,
    query: &str,
    held: &mut Held,
    shared: &Arc<Shared>,
    kitchen: &mut super::extended::Kitchen,
) -> Result<()> {
    let tx = &mut kitchen.tx;
    match super::session::parse(query) {
        Ok(Some(super::session::Session::Namespace(namespace))) => {
            held.set_namespace(&namespace);
            conn.send(&codec::write::command_complete("SET")).await?;
            conn.send(&codec::write::ready_for_query(*tx)).await?;
            return Ok(());
        }
        Ok(Some(super::session::Session::Transaction(verb))) => {
            transaction(conn, verb, tx).await?;
            return Ok(());
        }
        Ok(Some(super::session::Session::ShowNamespace)) => {
            let column = codec::write::Column {
                name: "search_path".to_owned(),
                type_oid: super::types::oid::TEXT,
                type_len: -1,
                format: 0,
            };
            conn.send(&codec::write::row_description(std::slice::from_ref(
                &column,
            )))
            .await?;
            conn.send(&codec::write::data_row(&[Some(
                held.namespace().as_bytes().to_vec(),
            )]))
            .await?;
            conn.send(&codec::write::command_complete("SHOW")).await?;
            conn.send(&codec::write::ready_for_query(*tx)).await?;
            return Ok(());
        }
        Ok(None) => {}
        Err(refused) => {
            if *tx == codec::write::Tx::InTx {
                *tx = codec::write::Tx::Failed;
            }
            conn.refuse(&refused, *tx).await?;
            return Ok(());
        }
    }
    if query.trim().is_empty() {
        conn.send(&codec::write::empty_query()).await?;
        conn.send(&codec::write::ready_for_query(*tx)).await?;
        return Ok(());
    }
    let translated = match translate::translate_many(query) {
        Ok(translated) => translated,
        Err(refused) => {
            if *tx == codec::write::Tx::InTx {
                *tx = codec::write::Tx::Failed;
            }
            conn.refuse(&refused, *tx).await?;
            return Ok(());
        }
    };
    if *tx == codec::write::Tx::Failed {
        conn.refuse(
            &Refused::surface(
                code::ABORTED,
                "current transaction is aborted, commands ignored until end of transaction block",
            ),
            *tx,
        )
        .await?;
        return Ok(());
    }
    // Catalog statements answer locally, never against the store — except
    // the user-table relations, which the store's own introspection builds
    // for this caller (every row it names is one this connection may see).
    // A batch that mixes catalog with store statements is refused by name:
    // the two run on different engines, and one implicit block must not
    // span both.
    if translated.statements.iter().any(|s| s.catalog.is_some()) {
        if !translated.statements.iter().all(|s| s.catalog.is_some()) {
            conn.refuse(
                &Refused::surface(
                    code::FEATURE,
                    "catalog statements run alone in this build; the batch mixes them with store reads",
                ),
                *tx,
            )
            .await?;
            return Ok(());
        }
        let queries = translated
            .statements
            .iter()
            .filter_map(|shape| shape.catalog.as_ref())
            .collect::<Vec<_>>();
        let dynamic = if translate::catalog::dynamic::needed(&queries) {
            match translate::catalog::dynamic::load(
                held,
                shared,
                translate::catalog::dynamic::wants_attributes(&queries),
                queries
                    .iter()
                    .find_map(|query| query.pinned_oid(&translated.slots, &translated.parameters)),
            )
            .await
            {
                Ok(dynamic) => dynamic,
                Err(refused) => {
                    if *tx == codec::write::Tx::InTx {
                        *tx = codec::write::Tx::Failed;
                    }
                    conn.refuse(&refused, *tx).await?;
                    return Ok(());
                }
            }
        } else {
            None
        };
        for shape in &translated.statements {
            let Some(query) = &shape.catalog else {
                continue;
            };
            let rows = match translate::catalog::evaluate(
                query,
                &translated.slots,
                &translated.parameters,
                dynamic.as_ref(),
                held.user(),
            ) {
                Ok(rows) => rows,
                Err(refused) => {
                    if *tx == codec::write::Tx::InTx {
                        *tx = codec::write::Tx::Failed;
                    }
                    conn.refuse(&refused, *tx).await?;
                    return Ok(());
                }
            };
            answer_catalog(conn, query, &rows, false, &[]).await?;
        }
        conn.send(&codec::write::ready_for_query(*tx)).await?;
        return Ok(());
    }
    let outcomes = match held
        .run(shared, &translated.script, &translated.parameters)
        .await
    {
        Ok(outcomes) => outcomes,
        Err(Failed::Refused(refused)) => {
            if *tx == codec::write::Tx::InTx {
                *tx = codec::write::Tx::Failed;
            }
            conn.refuse(&Refused::store(&refused), *tx).await?;
            return Ok(());
        }
        Err(Failed::Carried(carried)) => {
            if *tx == codec::write::Tx::InTx {
                *tx = codec::write::Tx::Failed;
            }
            conn.refuse(&Refused::carried(carried.class, carried.message), *tx)
                .await?;
            return Ok(());
        }
        Err(Failed::Busy) => {
            conn.refuse(
                &Refused::surface(code::UNAVAILABLE, "the node is busy; try again"),
                *tx,
            )
            .await?;
            return Ok(());
        }
        Err(Failed::Lost) => return Err(Error::Malformed("the session is lost")),
    };
    // The implicit block's BEGIN and COMMIT answer Done among the outcomes;
    // the statements pair with their shapes in order, the Dones skipped.
    let mut shapes = translated.statements.iter();
    for outcome in &outcomes {
        if matches!(outcome, tessaridb::Outcome::Done) {
            continue;
        }
        let Some(shape) = shapes.next() else {
            break;
        };
        // An encoding refusal was already answered on the wire; the
        // readiness below still arrives, as the simple protocol counts it.
        let _answered = answer(conn, outcome, shape, false, &[]).await?;
    }
    conn.send(&codec::write::ready_for_query(*tx)).await?;
    Ok(())
}

/// A catalog statement's rows as the wire spells them: the projection's
/// static column types, the values in text or the binary subset a Bind
/// asked for. As [`answer`], `described` marks a portal Describe that
/// already owed the row description.
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

/// One transaction verb, walked through the connection's block state the way
/// the dialect's own clients expect: COMMIT of an aborted block answers
/// ROLLBACK, a verb outside any block refuses by name, and BEGIN inside one
/// warns and changes nothing.
async fn transaction(
    conn: &mut Conn,
    verb: super::session::TxVerb,
    tx: &mut codec::write::Tx,
) -> Result<()> {
    use super::session::TxVerb;
    let (tag, reply): (&str, std::result::Result<(), Refused>) = match verb {
        TxVerb::Begin => match tx {
            codec::write::Tx::Idle => {
                *tx = codec::write::Tx::InTx;
                ("BEGIN", Ok(()))
            }
            _ => {
                conn.send(&codec::write::notice(
                    code::NO_TRANSACTION,
                    "there is already a transaction in progress",
                ))
                .await?;
                ("BEGIN", Ok(()))
            }
        },
        TxVerb::Commit => match tx {
            codec::write::Tx::InTx => {
                *tx = codec::write::Tx::Idle;
                ("COMMIT", Ok(()))
            }
            codec::write::Tx::Failed => {
                *tx = codec::write::Tx::Idle;
                ("ROLLBACK", Ok(()))
            }
            codec::write::Tx::Idle => (
                "COMMIT",
                Err(Refused::surface(
                    code::NO_TRANSACTION,
                    "there is no transaction in progress",
                )),
            ),
        },
        TxVerb::Rollback => match tx {
            codec::write::Tx::InTx | codec::write::Tx::Failed => {
                *tx = codec::write::Tx::Idle;
                ("ROLLBACK", Ok(()))
            }
            codec::write::Tx::Idle => (
                "ROLLBACK",
                Err(Refused::surface(
                    code::NO_TRANSACTION,
                    "there is no transaction in progress",
                )),
            ),
        },
    };
    match reply {
        Ok(()) => conn.send(&codec::write::command_complete(tag)).await?,
        Err(refused) => {
            conn.send(&codec::write::error(
                refused.code,
                &refused.message,
                refused.detail.as_deref(),
            ))
            .await?;
        }
    }
    conn.send(&codec::write::ready_for_query(*tx)).await?;
    Ok(())
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
    match outcome {
        Outcome::Records { records, .. } => {
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
            conn.send(&codec::write::command_complete(&format!(
                "SELECT {}",
                rows.len()
            )))
            .await?;
        }
        Outcome::Value(value) => {
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
            conn.send(&codec::write::command_complete("SELECT 0"))
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

/// The frame an error refuses with.
fn refused_frame(refused: &Refused) -> Vec<u8> {
    codec::write::error(refused.code, &refused.message, refused.detail.as_deref())
}

/// The namespace an `options` startup string names, when it does: the
/// dialect's own convention carries `-c name=value` pairs there, so a libpq
/// `options='-c namespace=app'` reaches the tenancy the same way a pair does.
fn option_namespace(options: &str) -> Option<String> {
    let mut words = options.split_whitespace();
    while let Some(word) = words.next() {
        let setting = if word == "-c" {
            words.next()?
        } else if let Some(rest) = word.strip_prefix("-c") {
            rest
        } else {
            continue;
        };
        let Some((name, value)) = setting.split_once('=') else {
            continue;
        };
        if name == "namespace" && !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

/// The stream a connection speaks over: plain, or TLS after the client asked
/// for it. Reads and writes share one type so the protocol loop never cares
/// which it holds.
pub(crate) enum Conn {
    Plain(
        tokio::io::ReadHalf<TcpStream>,
        tokio::io::WriteHalf<TcpStream>,
    ),
    Tls(
        tokio::io::ReadHalf<TlsStream<TcpStream>>,
        tokio::io::WriteHalf<TlsStream<TcpStream>>,
    ),
}

impl Conn {
    /// The first packet decides whether TLS wraps the stream; the startup
    /// packet itself is handed back buffered so nothing it said is read
    /// twice.
    async fn accept(
        stream: TcpStream,
        secured: Option<TlsAcceptor>,
    ) -> Result<(Self, Option<First>)> {
        let mut plain = stream;
        let payload = read_packet(&mut plain).await?;
        let first = codec::read::first(&payload)?;
        match (first, secured) {
            (First::Ssl, Some(acceptor)) => {
                plain.write_all(b"S").await?;
                let tls = acceptor.accept(plain).await?;
                let (read, write) = tokio::io::split(tls);
                Ok((Self::Tls(read, write), None))
            }
            (First::Ssl, None) => {
                plain.write_all(b"N").await?;
                let (read, write) = tokio::io::split(plain);
                Ok((Self::Plain(read, write), None))
            }
            (first, _) => {
                let (read, write) = tokio::io::split(plain);
                Ok((Self::Plain(read, write), Some(first)))
            }
        }
    }

    /// The first packet on a stream that negotiated TLS: the client sends the
    /// startup inside the negotiated channel.
    async fn first(&mut self) -> Result<First> {
        let payload = match self {
            Self::Plain(read, _) => read_packet(read).await?,
            Self::Tls(read, _) => read_packet(read).await?,
        };
        codec::read::first(&payload)
    }

    /// One message of a running session.
    async fn message(&mut self) -> Result<Frontend> {
        let (kind, payload) = match self {
            Self::Plain(read, _) => read_frame(read).await?,
            Self::Tls(read, _) => read_frame(read).await?,
        };
        codec::read::message(kind, &payload)
    }

    pub(crate) async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Plain(_, write) => {
                write.write_all(bytes).await?;
                write.flush().await?;
            }
            Self::Tls(_, write) => {
                write.write_all(bytes).await?;
                write.flush().await?;
            }
        }
        Ok(())
    }

    /// An error before the session starts: the client expects the connection
    /// to close after it, and it does.
    async fn refuse_startup(&mut self, refused: &Refused) -> Result<()> {
        self.send(&refused_frame(refused)).await
    }

    /// An error mid-session, followed by readiness as the simple query
    /// protocol requires — the transaction state rides it.
    pub(crate) async fn refuse(&mut self, refused: &Refused, tx: codec::write::Tx) -> Result<()> {
        self.send(&refused_frame(refused)).await?;
        self.send(&codec::write::ready_for_query(tx)).await
    }

    /// An error whose readiness waits for the Sync the client sends next —
    /// the extended protocol's error-recovery shape, so exactly one Ready
    /// answers one Sync.
    pub(crate) async fn refuse_pending(&mut self, refused: &Refused) -> Result<()> {
        self.send(&refused_frame(refused)).await
    }
}

/// Read one first packet: the length word, then the payload, the ceiling
/// checked before anything is allocated.
async fn read_packet(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len);
    if !(8..=crate::error::CEILING).contains(&len) {
        return Err(Error::Ceiling(len));
    }
    let payload_len = usize::try_from(len.saturating_sub(4))
        .map_err(|_| Error::Malformed("the length does not fit"))?;
    let mut payload = vec![0_u8; payload_len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Read one message: the kind byte, the length word, the payload.
async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<(u8, Vec<u8>)> {
    let mut head = [0_u8; 5];
    stream.read_exact(&mut head).await?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    if !(4..=crate::error::CEILING).contains(&len) {
        return Err(Error::Ceiling(len));
    }
    let payload_len = usize::try_from(len.saturating_sub(4))
        .map_err(|_| Error::Malformed("the length does not fit"))?;
    let mut payload = vec![0_u8; payload_len];
    stream.read_exact(&mut payload).await?;
    Ok((head[0], payload))
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(read, _) => Pin::new(read).poll_read(cx, buf),
            Self::Tls(read, _) => Pin::new(read).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(_, write) => Pin::new(write).poll_write(cx, buf),
            Self::Tls(_, write) => Pin::new(write).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(_, write) => Pin::new(write).poll_flush(cx),
            Self::Tls(_, write) => Pin::new(write).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(_, write) => Pin::new(write).poll_shutdown(cx),
            Self::Tls(_, write) => Pin::new(write).poll_shutdown(cx),
        }
    }
}
