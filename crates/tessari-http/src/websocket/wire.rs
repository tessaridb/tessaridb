//! `GET /wire` — the wire protocol over a WebSocket (ADR-0089).
//!
//! This route does not speak the protocol. It proves the upgrade, then pumps
//! bytes between the socket and a pipe whose other end is a session of the wire
//! node — the same session a TCP connection gets, counted at the same door. The
//! pump reads what the protocol leaves to the carrier and nothing more: binary
//! messages are the byte stream and their boundaries mean nothing, a text
//! message is not part of it, and a close ends it.
//!
//! # Two directions, pumped independently
//!
//! A client may send its next statement before reading the last answer. If one
//! loop did both, a large answer filling the pipe would stop the loop from
//! reading the socket while the session, waiting to write, stopped reading the
//! pipe — each waiting on the other. So each direction has its own loop and the
//! first to end ends both.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tessari_constants::WIRE_SOCKET_MAX_MESSAGE_BYTES;
use tessari_serve::{Admitted, Busy};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

use super::{close_code, echo_the_close, end, handshake, refused};
use crate::{Shared, WireSession};

/// How many bytes the pipe holds in each direction before a writer waits.
const PIPE_BYTES: usize = 64 * 1024;

/// Upgrade `GET /wire`, or refuse it in this route's own words.
pub(crate) async fn wire(
    node: Arc<Shared>,
    parts: &mut Parts,
    busy: Busy,
    place: Admitted,
) -> Response {
    let Some(door) = node.wire.clone() else {
        node.stopping.answered(true);
        return refused(
            404,
            r#"{"error":"this node does not serve the wire protocol; start it with --serve"}"#,
        );
    };
    if let Err(refusal) = handshake::check(&parts.headers) {
        node.stopping.answered(true);
        let (status, body) = refusal.answer();
        return refused(status, body);
    }
    let upgrade = match WebSocketUpgrade::from_request_parts(parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => {
            node.stopping.answered(true);
            return rejection.into_response();
        }
    };
    // Before the `101`, so a full node says so in a status a client can read
    // rather than by upgrading and hanging up.
    let Some(session) = door() else {
        node.stopping.answered(true);
        return refused(
            503,
            r#"{"error":"this node is serving as many connections as it will"}"#,
        );
    };
    node.stopping.answered(false);
    // The request is answered at the `101`. From there the session is the wire
    // node's, counted by its door and its drain, so this surface's own place is
    // given back rather than held for a socket it no longer serves.
    drop((busy, place));
    upgrade
        .max_message_size(WIRE_SOCKET_MAX_MESSAGE_BYTES)
        .max_frame_size(WIRE_SOCKET_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| carry(socket, session))
}

/// Run the session and the pump until both have ended.
async fn carry(socket: WebSocket, session: WireSession) {
    let (pipe, theirs) = tokio::io::duplex(PIPE_BYTES);
    // Whichever ends first drops its end of the pipe, which is how the other
    // learns: the session reads the end of its stream, or the pump reads it.
    tokio::join!(session(theirs), pump(socket, pipe));
}

/// How the pump ended, and what is still owed to the client.
enum Ended {
    /// Close with this code.
    Close(u16),
    /// The client closed; read on so the echo goes out.
    Echo,
    /// Nobody is left to say anything to.
    Gone,
}

async fn pump(socket: WebSocket, pipe: DuplexStream) {
    let (from_node, to_node) = tokio::io::split(pipe);
    let (mut sink, mut stream) = socket.split();
    let ended = tokio::select! {
        ended = upward(&mut stream, to_node) => ended,
        ended = downward(&mut sink, from_node) => ended,
    };
    let Ok(mut socket) = sink.reunite(stream) else {
        return;
    };
    match ended {
        Ended::Close(code) => end(&mut socket, code).await,
        Ended::Echo => echo_the_close(&mut socket).await,
        Ended::Gone => {}
    }
}

/// What the client sends, into the session.
async fn upward(
    stream: &mut SplitStream<WebSocket>,
    mut to_node: WriteHalf<DuplexStream>,
) -> Ended {
    loop {
        match stream.next().await {
            None => return Ended::Gone,
            Some(Err(why)) => return Ended::Close(close_code(&why)),
            Some(Ok(Message::Close(_))) => return Ended::Echo,
            // Not part of the protocol, and closing says so rather than guessing
            // what a string was meant to be.
            Some(Ok(Message::Text(_))) => return Ended::Close(1003),
            Some(Ok(Message::Binary(bytes))) => {
                if to_node.write_all(&bytes).await.is_err() {
                    // The session ended; the other loop reads that and closes.
                    return Ended::Close(1000);
                }
            }
            // Answered by the protocol layer.
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        }
    }
}

/// What the session writes, out to the client.
async fn downward(
    sink: &mut SplitSink<WebSocket, Message>,
    mut from_node: ReadHalf<DuplexStream>,
) -> Ended {
    let mut buffer = vec![0u8; PIPE_BYTES];
    loop {
        let read = match from_node.read(&mut buffer).await {
            // The session is over: a normal close.
            Ok(0) | Err(_) => return Ended::Close(1000),
            Ok(read) => read,
        };
        let Some(bytes) = buffer.get(..read) else {
            return Ended::Close(1011);
        };
        if sink
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .is_err()
        {
            return Ended::Gone;
        }
    }
}
