//! `GET /backup` as a stream (ADR-0094 D6): the snapshot leaves the node chunk
//! by chunk as it is read, so the node's memory does not grow with the store.
//!
//! # Who may take one is still the statement's question
//!
//! The work runs `BACKUP STATE;` through a session like every other route, with
//! the session told to write its snapshot into this response instead of
//! answering with it. So the same check that refuses a database owner refuses
//! one here, before a byte is written — and the status is decided by what
//! arrives first: a chunk means the statement was allowed and the file is under
//! way, and a refusal can only arrive before any chunk could.
//!
//! # What a failure halfway through looks like
//!
//! The status has been sent by then, so the body is cut off. A snapshot ends in
//! a frame that counts its chunks and records, and a file without it verifies as
//! incomplete and restores nothing (ADR-0091) — a cut stream cannot pass for a
//! whole backup.

use std::io::Write;
use std::sync::Arc;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tessari_serve::{Admitted, Bridged, Busy};
use tokio::sync::mpsc;

use crate::basic;
use crate::respond::{Answer, OCTETS, failure, session_for};
use crate::{Shared, to_response};

/// How many bytes one chunk of the body carries.
const CHUNK_BYTES: usize = 64 * 1024;

/// How many chunks may wait between the read and the socket — the bound that
/// keeps a slow client from making the node hold the store in memory.
const CHUNKS_IN_FLIGHT: usize = 4;

/// What the work hands the route, in order.
enum Piece {
    /// Some of the file.
    Bytes(Vec<u8>),
    /// The statement was refused — before any byte, or, after one, the reason
    /// the body is cut short.
    Refused(Answer),
}

/// Answer `GET /backup` (or `?as=state`) with the snapshot as it is read.
pub(crate) async fn backup(
    node: Arc<Shared>,
    parts: &Parts,
    busy: Busy,
    place: Admitted,
) -> Response {
    let presented = basic::presented(
        parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    );
    let (pieces, mut received) = mpsc::channel::<Piece>(CHUNKS_IN_FLIGHT);
    let working = Arc::clone(&node);
    let busy_told = pieces.clone();
    tokio::spawn(async move {
        let bridged = working
            .bridge
            .call(Arc::clone(&working), move |node| {
                let out = Chunks {
                    out: pieces.clone(),
                    held: Vec::with_capacity(CHUNK_BYTES),
                };
                let ran = session_for(&node.db, &node.tokens, &presented).and_then(|session| {
                    session
                        .snapshot_into(Box::new(out))
                        .run("BACKUP STATE;")
                        .map_err(|error| failure(&error))
                });
                // A refusal before the first byte is the route's answer; one
                // after it can only cut the body short.
                if let Err(answer) = ran {
                    drop(pieces.blocking_send(Piece::Refused(answer)));
                }
            })
            .await;
        if let Bridged::Busy(_) | Bridged::Panicked = bridged {
            drop(
                busy_told
                    .send(Piece::Refused(Answer::new(
                        503,
                        r#"{"error":"this node is answering as many requests as it will"}"#
                            .to_owned(),
                    )))
                    .await,
            );
        }
        // Held until the work is done, as a request's place is held until it
        // is answered.
        drop((busy, place));
    });
    let first = match received.recv().await {
        Some(Piece::Bytes(bytes)) => bytes,
        Some(Piece::Refused(answer)) => {
            node.stopping.answered(answer.status >= 400);
            return to_response(answer);
        }
        None => {
            node.stopping.answered(true);
            return to_response(Answer::new(
                500,
                r#"{"error":"the backup ended before it wrote anything"}"#.to_owned(),
            ));
        }
    };
    node.stopping.answered(false);
    let mut first = Some(first);
    let body = Body::from_stream(futures_util::stream::poll_fn(move |context| {
        if let Some(bytes) = first.take() {
            return std::task::Poll::Ready(Some(Ok(bytes)));
        }
        received.poll_recv(context).map(|piece| match piece {
            Some(Piece::Bytes(bytes)) => Some(Ok(bytes)),
            Some(Piece::Refused(answer)) => Some(Err(std::io::Error::other(
                String::from_utf8_lossy(&answer.body).into_owned(),
            ))),
            None => None,
        })
    }));
    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(OCTETS));
    response
}

/// The body's writer: bytes gathered into chunks and handed to the socket.
struct Chunks {
    out: mpsc::Sender<Piece>,
    held: Vec<u8>,
}

impl Chunks {
    fn send(&mut self) -> std::io::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.held, Vec::with_capacity(CHUNK_BYTES));
        self.out.blocking_send(Piece::Bytes(chunk)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the caller stopped reading the backup",
            )
        })
    }
}

impl Write for Chunks {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.held.extend_from_slice(bytes);
        if self.held.len() >= CHUNK_BYTES {
            self.send()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}
