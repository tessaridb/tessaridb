//! `GET /backup` without holding the snapshot in memory (ADR-0094 D6).
//!
//! # Spooled, not chunked
//!
//! The protocol requires every response to declare its length and forbids
//! chunked framing on every route, `/backup` by name (protocol §5.3), and a
//! conforming client refuses anything else. A snapshot's length is not known
//! until it has been written, so it is written first — into a file in the
//! node's temporary folder, unlinked the moment it is open, so nothing is left
//! behind whatever happens next — and then sent from that file with its exact
//! length. Memory stays flat; the cost is room on disk for one snapshot and a
//! wait before the first byte.
//!
//! # Who may take one is still the statement's question
//!
//! The work runs `BACKUP STATE;` through a session like every other route, with
//! the session told to write its snapshot into the file instead of answering
//! with it. The same check that refuses a database owner refuses one here,
//! before anything is written, and that refusal is the status the caller gets.

use std::io::{Read, Seek};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tessari_serve::{Admitted, Bridged, Busy};
use tokio::sync::mpsc;

use crate::basic;
use crate::respond::{Answer, OCTETS, failure, session_for};
use crate::{Shared, to_response};

/// How many bytes one piece of the body carries.
const CHUNK_BYTES: usize = 64 * 1024;

/// How many pieces may wait between the file and the socket — the bound that
/// keeps a slow client from making the node read the file into memory.
const CHUNKS_IN_FLIGHT: usize = 4;

/// Spools this process has made, so two at once never share a name. A count
/// and nothing else, so `Relaxed` is all it needs.
static SPOOLED: AtomicU64 = AtomicU64::new(0);

/// What the work hands the route, in order.
enum Piece {
    /// The snapshot is written and this long; its bytes follow.
    Length(u64),
    /// Some of the file.
    Bytes(Vec<u8>),
    /// The statement was refused — before the length, or, after it, the reason
    /// the body is cut short.
    Refused(Answer),
}

/// Answer `GET /backup` (or `?as=state`) with the snapshot, spooled to disk.
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
                let spooled = spool(&node, &presented);
                send(spooled, &pieces);
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
    let length = match received.recv().await {
        Some(Piece::Length(length)) => length,
        Some(Piece::Refused(answer)) => {
            node.stopping.answered(answer.status >= 400);
            return to_response(answer);
        }
        Some(Piece::Bytes(_)) | None => {
            node.stopping.answered(true);
            return to_response(Answer::new(
                500,
                r#"{"error":"the backup ended before it said how long it was"}"#.to_owned(),
            ));
        }
    };
    node.stopping.answered(false);
    let body = Body::from_stream(futures_util::stream::poll_fn(move |context| {
        received.poll_recv(context).map(|piece| match piece {
            Some(Piece::Bytes(bytes)) => Some(Ok(bytes)),
            Some(Piece::Refused(answer)) => Some(Err(std::io::Error::other(
                String::from_utf8_lossy(&answer.body).into_owned(),
            ))),
            Some(Piece::Length(_)) | None => None,
        })
    }));
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(OCTETS));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

/// Write the snapshot the caller may take into an unlinked file, and answer
/// the file and its length, or the refusal.
fn spool(node: &Shared, presented: &basic::Presented) -> Result<(std::fs::File, u64), Answer> {
    let unwritable = |why: &std::io::Error| {
        Answer::new(
            500,
            format!(
                r#"{{"error":"the node could not write the backup to its temporary folder: {}"}}"#,
                why.kind()
            ),
        )
    };
    let path = std::env::temp_dir().join(format!(
        "tessaridb-backup-{}-{}.tessarisnap",
        std::process::id(),
        SPOOLED.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|why| unwritable(&why))?;
    // Unlinked at once: the open handle keeps the bytes, and nothing outlives
    // this request whether it ends in a refusal, a cut connection or a crash.
    std::fs::remove_file(&path).map_err(|why| unwritable(&why))?;
    let writer = file.try_clone().map_err(|why| unwritable(&why))?;
    session_for(&node.db, &node.tokens, presented).and_then(|session| {
        session
            .snapshot_into(Box::new(std::io::BufWriter::new(writer)))
            .run("BACKUP STATE;")
            .map_err(|error| failure(&error))
    })?;
    let length = file
        .seek(std::io::SeekFrom::End(0))
        .and_then(|length| file.rewind().map(|()| length))
        .map_err(|why| unwritable(&why))?;
    Ok((file, length))
}

/// Hand the route the length and then the file, a piece at a time.
fn send(spooled: Result<(std::fs::File, u64), Answer>, pieces: &mpsc::Sender<Piece>) {
    let (mut file, length) = match spooled {
        Ok(spooled) => spooled,
        Err(answer) => {
            drop(pieces.blocking_send(Piece::Refused(answer)));
            return;
        }
    };
    if pieces.blocking_send(Piece::Length(length)).is_err() {
        return;
    }
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                let piece = buffer.get(..read).map(<[u8]>::to_vec).unwrap_or_default();
                // A caller that stopped reading has gone; nothing is left to do.
                if pieces.blocking_send(Piece::Bytes(piece)).is_err() {
                    return;
                }
            }
            Err(why) => {
                drop(pieces.blocking_send(Piece::Refused(Answer::new(
                    500,
                    format!(
                        r#"{{"error":"reading the spooled backup failed: {}"}}"#,
                        why.kind()
                    ),
                ))));
                return;
            }
        }
    }
}
