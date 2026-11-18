//! The surface served on an in-memory store and spoken to over a socket: a
//! hand-rolled client of the wire protocol, exactly the bytes a client sends,
//! read back as a client reads them.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::codec;

/// The fixture every socket test reads: a table with a spread of fields and
/// rows, one of them missing the optional one. The fields are declared so
/// the catalog the port reads at Parse time names their types.
pub(crate) const SETUP: &str = "DEFINE TABLE orders SCHEMALESS; \
     DEFINE FIELD band ON orders TYPE int; \
     DEFINE FIELD name ON orders TYPE string; \
     CREATE orders:1 = { band: 1, name: 'first' }; \
     CREATE orders:2 = { band: 3, name: 'third' }; \
     CREATE orders:3 = { band: 3 };";

mod agreement;
mod binary;
mod catalog;
mod extended;
mod matrix;
mod queries;
mod secured;
mod signing;
mod tenancy;
mod transaction;
mod widening;

/// A node over a fresh store holding the namespace `app` and the database
/// `main`, after `setup` ran on it.
async fn serving_with(setup: &str) -> (String, CancellationToken, Arc<tessaridb::Db>) {
    serving_adjusted(setup, |node| node).await
}

/// As [`serving_with`], the node changed by `adjust` before it serves.
async fn serving_adjusted(
    setup: &str,
    adjust: impl FnOnce(super::Node) -> super::Node,
) -> (String, CancellationToken, Arc<tessaridb::Db>) {
    let db = Arc::new(
        tessaridb::Db::in_memory()
            .await
            .expect("an in-memory store"),
    );
    let mut session = db.session();
    session
        .run("DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; USE DATABASE main;")
        .await
        .expect("the base tenancy defines");
    if !setup.is_empty() {
        session.run(setup).await.expect("the setup script runs");
    }
    drop(session);
    let node =
        adjust(super::Node::bind(Arc::clone(&db), "127.0.0.1:0").expect("an ephemeral port"));
    let address = node.address().expect("the bound address");
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move { node.serve(stopping).await });
    (address, stop, db)
}

/// One message read back, decoded just far enough for an assertion.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    Auth(u32),
    Param(String, String),
    BackendKey,
    Ready(u8),
    RowDesc(Vec<String>),
    DataRow(Vec<Option<String>>),
    Command(String),
    Empty,
    Error(String, String),
    /// A WARNING frame the answer carries beside the completion — the
    /// kept-values notice a dropped column owes.
    Notice(String, String),
    ParseComplete,
    BindComplete,
    CloseComplete,
    ParamDesc(Vec<u32>),
    NoData,
    Suspended,
}

/// A connected client speaking the wire protocol by hand.
pub(crate) struct Client {
    pub(crate) stream: TcpStream,
}

impl Client {
    async fn to(address: &str) -> Self {
        Self {
            stream: TcpStream::connect(address).await.expect("the node answers"),
        }
    }

    pub(crate) async fn write(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("the node reads");
    }

    pub(crate) async fn read(&mut self, n: usize) -> Vec<u8> {
        let mut out = vec![0_u8; n];
        tokio::time::timeout(Duration::from_secs(5), self.stream.read_exact(&mut out))
            .await
            .expect("a message arrives in time")
            .expect("the node answers");
        out
    }

    /// A startup packet from arbitrary pairs — the tenancy channels the
    /// namespace resolution reads ride here too.
    fn startup_packet(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&codec::PROTOCOL_VERSION.to_be_bytes());
        for (key, value) in pairs {
            push_pair(&mut body, key, value);
        }
        body.push(0);
        let mut frame = Vec::new();
        frame.extend_from_slice(
            &u32::try_from(body.len().saturating_add(4))
                .expect("a startup packet is small")
                .to_be_bytes(),
        );
        frame.extend_from_slice(&body);
        frame
    }

    /// Send the startup and answer the authentication cycle; returns the
    /// messages up to and including the first readiness.
    pub(crate) async fn start(
        &mut self,
        user: Option<&str>,
        database: &str,
        password: Option<&str>,
    ) -> Vec<Seen> {
        self.start_pairs(
            &[user.map(|u| ("user", u)), Some(("database", database))]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            password,
        )
        .await
    }

    /// As [`Self::start`], from arbitrary startup pairs.
    pub(crate) async fn start_pairs(
        &mut self,
        pairs: &[(&str, &str)],
        password: Option<&str>,
    ) -> Vec<Seen> {
        self.write(&Self::startup_packet(pairs)).await;
        let mut seen = Vec::new();
        loop {
            let Some(message) = self.try_message().await else {
                return seen;
            };
            let again = !matches!(message, Seen::Ready(_));
            seen.push(message);
            if let Some(Seen::Auth(3)) = seen.last() {
                // A cleartext challenge: the password answers it.
                let mut frame = vec![b'p'];
                let mut body = password.unwrap_or("").as_bytes().to_vec();
                body.push(0);
                frame.extend_from_slice(
                    &u32::try_from(body.len().saturating_add(4))
                        .expect("a password packet is small")
                        .to_be_bytes(),
                );
                frame.extend_from_slice(&body);
                self.write(&frame).await;
            }
            if !again {
                return seen;
            }
        }
    }

    /// One query, read to its readiness.
    pub(crate) async fn query(&mut self, sql: &str) -> Vec<Seen> {
        let mut frame = vec![b'Q'];
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        frame.extend_from_slice(
            &u32::try_from(body.len().saturating_add(4))
                .expect("a query packet is small")
                .to_be_bytes(),
        );
        frame.extend_from_slice(&body);
        self.write(&frame).await;
        let mut seen = Vec::new();
        loop {
            let message = self.message().await;
            let ready = matches!(message, Seen::Ready(_));
            seen.push(message);
            if ready {
                return seen;
            }
        }
    }

    /// A Parse: a named statement and the parameter types it declares.
    pub(crate) async fn parse(&mut self, name: &str, sql: &str, oids: &[u32]) {
        let mut body = cstrs(name);
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        put_u16(&mut body, oids.len());
        for oid in oids {
            body.extend_from_slice(&oid.to_be_bytes());
        }
        self.write(&framed(b'P', body)).await;
    }

    /// A Bind: a portal over a statement, the parameter formats and values,
    /// and the result formats the client asks for.
    pub(crate) async fn bind(
        &mut self,
        portal: &str,
        statement: &str,
        formats: &[u16],
        params: &[Option<Vec<u8>>],
        results: &[u16],
    ) {
        let mut body = cstrs(portal);
        body.extend_from_slice(statement.as_bytes());
        body.push(0);
        put_u16(&mut body, formats.len());
        for format in formats {
            body.extend_from_slice(&format.to_be_bytes());
        }
        put_u16(&mut body, params.len());
        for param in params {
            match param {
                None => body.extend_from_slice(&(-1_i32).to_be_bytes()),
                Some(bytes) => {
                    body.extend_from_slice(
                        &i32::try_from(bytes.len()).unwrap_or(i32::MAX).to_be_bytes(),
                    );
                    body.extend_from_slice(bytes);
                }
            }
        }
        put_u16(&mut body, results.len());
        for format in results {
            body.extend_from_slice(&format.to_be_bytes());
        }
        self.write(&framed(b'B', body)).await;
    }

    /// A Describe over a statement (`b'S'`) or a portal (`b'P'`).
    pub(crate) async fn describe(&mut self, kind: u8, name: &str) {
        let mut body = vec![kind];
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        self.write(&framed(b'D', body)).await;
    }

    /// An Execute of a portal, at most `rows` rows asked for.
    pub(crate) async fn execute(&mut self, portal: &str, rows: u32) {
        let mut body = cstrs(portal);
        body.extend_from_slice(&rows.to_be_bytes());
        self.write(&framed(b'E', body)).await;
    }

    /// A Close of a statement or a portal.
    pub(crate) async fn close(&mut self, kind: u8, name: &str) {
        let mut body = vec![kind];
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        self.write(&framed(b'C', body)).await;
    }

    /// A Sync: the end of the extended batch; readiness answers it.
    pub(crate) async fn sync(&mut self) {
        self.write(&framed(b'S', Vec::new())).await;
    }

    /// One raw message.
    pub(crate) async fn message(&mut self) -> Seen {
        self.try_message()
            .await
            .expect("a message arrives before the connection closes")
    }

    /// One raw message, or `None` when the connection closed first.
    pub(crate) async fn try_message(&mut self) -> Option<Seen> {
        let mut head = [0_u8; 5];
        let read = tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut head))
            .await
            .expect("a head arrives in time")
            .expect("the socket reads");
        if read == 0 {
            return None;
        }
        let mut filled = read;
        while filled < 5 {
            let more = tokio::time::timeout(
                Duration::from_secs(5),
                self.stream.read(&mut head[filled..]),
            )
            .await
            .expect("the rest of the head arrives")
            .expect("the socket reads");
            if more == 0 {
                return None;
            }
            filled = filled.saturating_add(more);
        }
        let len = u32::from_be_bytes(head[1..].try_into().expect("four length bytes"));
        let len = usize::try_from(len.saturating_sub(4)).expect("the length is at least 4");
        let mut body = vec![0_u8; len];
        let mut got = 0;
        while got < len {
            let more =
                tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut body[got..]))
                    .await
                    .expect("the body arrives in time")
                    .expect("the socket reads");
            if more == 0 {
                return None;
            }
            got = got.saturating_add(more);
        }
        Some(decode(head[0], &body))
    }
}

fn push_pair(body: &mut Vec<u8>, key: &str, value: &str) {
    body.extend_from_slice(key.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
}

/// One null-terminated string.
fn cstrs(text: &str) -> Vec<u8> {
    let mut out = text.as_bytes().to_vec();
    out.push(0);
    out
}

fn put_u16(body: &mut Vec<u8>, n: usize) {
    let n = u16::try_from(n).expect("a count this small");
    body.extend_from_slice(&n.to_be_bytes());
}

/// One frame: the kind byte, the length word counting itself and the body,
/// then the body.
fn framed(kind: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(
        &u32::try_from(body.len().saturating_add(4))
            .expect("a frame this small")
            .to_be_bytes(),
    );
    out.extend_from_slice(&body);
    out
}

/// Decode one message far enough for the assertions; anything unspoken keeps
/// its kind byte in the tag.
pub(crate) fn decode(kind: u8, body: &[u8]) -> Seen {
    match kind {
        b'R' => Seen::Auth(u32::from_be_bytes(
            body[..4].try_into().expect("four bytes"),
        )),
        b'S' => {
            let (name, rest) = split(body);
            let (value, _) = split(rest);
            Seen::Param(name, value)
        }
        b'K' => Seen::BackendKey,
        b'Z' => Seen::Ready(body[0]),
        b'T' => {
            let count = u16::from_be_bytes(body[..2].try_into().expect("two bytes"));
            let mut at: usize = 2;
            let mut names = Vec::new();
            for _ in 0..count {
                let (name, rest) = split(&body[at..]);
                names.push(name);
                // table oid 4, attnum 2, type oid 4, len 2, typmod 4, format 2.
                at = body.len().saturating_sub(rest.len()).saturating_add(18);
            }
            Seen::RowDesc(names)
        }
        b'D' => {
            let count = u16::from_be_bytes(body[..2].try_into().expect("two bytes"));
            let mut at: usize = 2;
            let mut cells = Vec::new();
            for _ in 0..count {
                let len = i32::from_be_bytes(
                    body[at..at.saturating_add(4)]
                        .try_into()
                        .expect("four length bytes"),
                );
                at = at.saturating_add(4);
                if len < 0 {
                    cells.push(None);
                } else {
                    let n = usize::try_from(len).expect("a positive length fits usize");
                    cells.push(Some(
                        String::from_utf8(body[at..at.saturating_add(n)].to_vec())
                            .expect("a cell is utf-8 text"),
                    ));
                    at = at.saturating_add(n);
                }
            }
            Seen::DataRow(cells)
        }
        b'C' => Seen::Command(cstr(body)),
        b's' => Seen::Suspended,
        b'I' => Seen::Empty,
        b'1' => Seen::ParseComplete,
        b'2' => Seen::BindComplete,
        b'3' => Seen::CloseComplete,
        b'n' => Seen::NoData,
        b't' => {
            let count = u16::from_be_bytes(body[..2].try_into().expect("two bytes"));
            let mut at: usize = 2;
            let mut oids = Vec::new();
            for _ in 0..count {
                oids.push(u32::from_be_bytes(
                    body[at..at.saturating_add(4)]
                        .try_into()
                        .expect("four bytes"),
                ));
                at = at.saturating_add(4);
            }
            Seen::ParamDesc(oids)
        }
        b'E' | b'N' => {
            let fields = fields(body);
            let code = fields
                .iter()
                .find(|(k, _)| *k == b'C')
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let message = fields
                .iter()
                .find(|(k, _)| *k == b'M')
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            if kind == b'E' {
                Seen::Error(code, message)
            } else {
                Seen::Notice(code, message)
            }
        }
        other => Seen::Command(format!("unspoken {other}")),
    }
}

fn fields(body: &[u8]) -> Vec<(u8, String)> {
    let mut rest = body;
    let mut out = Vec::new();
    while !rest.is_empty() && rest[0] != 0 {
        let kind = rest[0];
        let (value, tail) = split(&rest[1..]);
        out.push((kind, value));
        rest = tail;
    }
    out
}

fn cstr(body: &[u8]) -> String {
    let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
    String::from_utf8(body[..end].to_vec()).expect("a tag is utf-8")
}

fn split(body: &[u8]) -> (String, &[u8]) {
    let end = body
        .iter()
        .position(|b| *b == 0)
        .expect("a cstring terminates");
    (
        String::from_utf8(body[..end].to_vec()).expect("a string is utf-8"),
        &body[end.saturating_add(1)..],
    )
}

/// The ParameterStatus map from a startup's messages.
pub(crate) fn params(seen: &[Seen]) -> Vec<(String, String)> {
    seen.iter()
        .filter_map(|message| match message {
            Seen::Param(name, value) => Some((name.clone(), value.clone())),
            _ => None,
        })
        .collect()
}

/// The rows a query's messages carry.
pub(crate) fn rows(seen: &[Seen]) -> Vec<Vec<Option<String>>> {
    seen.iter()
        .filter_map(|message| match message {
            Seen::DataRow(cells) => Some(cells.clone()),
            _ => None,
        })
        .collect()
}

/// The first ErrorResponse a query's messages carry.
pub(crate) fn refused(seen: &[Seen]) -> Option<(String, String)> {
    seen.iter().find_map(|message| match message {
        Seen::Error(code, message) => Some((code.clone(), message.clone())),
        _ => None,
    })
}
mod writes;
