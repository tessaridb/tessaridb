//! `GET /watch` — the route a browser upgrades on.
//!
//! ADR-0016 decided one route where `GET` upgrades to a WebSocket carrying the
//! change feed. ADR-0085 §6 moved the protocol itself to axum: the handshake's
//! proof, the frames, pings and the size ceiling are its, and what stays here is
//! this route's contract — the refusals before an upgrade (`handshake.rs`), the
//! one text message that says what to follow, and the close codes after it.
//!
//! # An upgraded socket is a feed, not a request
//!
//! The moment the handshake completes, this connection stops being something a
//! drain can wait for: it ends when the client says so or when the process does.
//! `tessari-serve` was built around exactly that distinction — a request finishes
//! on its own and a feed never does — so the connection **moves** to the feed
//! count here. Left in the request count, every shutdown would wait the full
//! drain deadline for a socket that was never going to close, which is the defect
//! the two counts exist to prevent.
//!
//! # A feed is a task that holds no thread
//!
//! Between rounds it waits on the commit signal, the socket and a timer at once,
//! as the wire feed does; each round crosses the node's rounds bridge because it
//! reads the store. A client that closes the socket is noticed there, with no
//! write needed to learn it.

mod follow;
mod handshake;
mod wire;

pub(crate) use wire::wire;

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tessari_constants::SOCKET_MAX_MESSAGE_BYTES;
use tessari_serve::{Admitted, Bridged, Busy};
use tessaridb::feed::{self, Feed, Following, Round};
use tessaridb::{Detached, Sequence};

use crate::Shared;
use crate::basic::{self, Credentials, Presented};

/// What a client is told when every slot that would open its feed is taken.
const BUSY: &str = "this node is answering as many requests as it will";

/// Upgrade `GET /watch`, or refuse it in this route's own words.
pub(crate) async fn watch(
    node: Arc<Shared>,
    parts: &mut Parts,
    mut busy: Busy,
    place: Admitted,
) -> Response {
    let presented = basic::presented(
        parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    );
    if let Err(refusal) = handshake::check(&parts.headers) {
        node.stopping.answered(true);
        let (status, body) = refusal.answer();
        return refused(status, body);
    }
    // Checked above, so axum accepts what reaches it; its own rejection is kept
    // only for a case the check above does not know about.
    let upgrade = match WebSocketUpgrade::from_request_parts(parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => {
            node.stopping.answered(true);
            return rejection.into_response();
        }
    };
    node.stopping.answered(false);
    busy.became_a_feed();
    upgrade
        .max_message_size(SOCKET_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            session(socket, &node, presented).await;
            // Held for the socket's whole life and released as it ends.
            drop((busy, place));
        })
}

/// A refusal before the upgrade, as JSON.
fn refused(status: u16, body: &'static str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Wait for the one text message that says what to follow, then follow it.
async fn session(mut socket: WebSocket, node: &Shared, presented: Presented) {
    let body = loop {
        match socket.recv().await {
            None => return,
            Some(Ok(Message::Close(_))) => return echo_the_close(&mut socket).await,
            Some(Err(why)) => return end(&mut socket, close_code(&why)).await,
            Some(Ok(Message::Text(text))) => break text.to_string(),
            // A binary message is not a request this route reads.
            Some(Ok(Message::Binary(_))) => return end(&mut socket, 1003).await,
            // Answered by the protocol layer.
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        }
    };
    follow(socket, node, presented, &body).await;
}

/// Open the feed the request names and push its changes until it ends.
async fn follow(mut socket: WebSocket, node: &Shared, presented: Presented, body: &str) {
    let asked = match follow::read(body) {
        Ok(asked) => asked,
        Err(reason) => return refuse_on(&mut socket, &reason).await,
    };
    // Who: the connection's own header if it carried one, else the request's —
    // a browser cannot set a header on a websocket, so it says so in the body.
    let claimed = match presented {
        Presented::Nobody => match (asked.token.clone(), asked.credentials.clone()) {
            (Some(bearer), _) => Presented::Token(bearer),
            (None, Some((name, password))) => Presented::Password(Credentials { name, password }),
            (None, None) => Presented::Nobody,
        },
        held => held,
    };
    let db = Arc::clone(&node.db);
    let tokens = Arc::clone(&node.tokens);
    // Signing in hashes a password and opening reads the catalog, so both cross
    // the bridge like any request.
    let opened = node
        .bridge
        .call((claimed, asked), move |(claimed, asked)| {
            let mut session = crate::respond::session_for(&db, &tokens, &claimed)
                .map_err(|answer| String::from_utf8_lossy(&answer.body).into_owned())?;
            if !crate::object::is_identifier(&asked.namespace)
                || !crate::object::is_identifier(&asked.database)
            {
                return Err("a namespace and database are plain names".to_owned());
            }
            let selecting = format!(
                "USE NAMESPACE {} DATABASE {};",
                asked.namespace, asked.database
            );
            session.run(&selecting).map_err(|error| error.to_string())?;
            let following = Following {
                from: Sequence::new(asked.from),
                table: asked.table.as_deref(),
                cursor: asked.cursor.as_deref(),
            };
            let opened =
                Feed::open(&db, &mut session, &following).map_err(|refusal| refusal.to_string())?;
            Ok::<_, String>((session.detach(), opened))
        })
        .await;
    let (mut session, mut following) = match opened {
        Bridged::Answered(Ok(opened)) => opened,
        Bridged::Answered(Err(reason)) => return refuse_on(&mut socket, &reason).await,
        Bridged::Busy(_) => return refuse_on(&mut socket, BUSY).await,
        Bridged::Panicked => return end(&mut socket, 1011).await,
    };
    let mut commits = node.committed.watching();
    // Whether the log may hold something this feed has not read: at the start,
    // and after every landing the store announces (Q-838).
    let mut due = true;
    loop {
        // A staged shutdown reaches a feed here, within one wait.
        if node.stopping.asked() {
            return end(&mut socket, 1001).await;
        }
        if due {
            // Marked seen BEFORE the round, so a landing during it wakes the wait.
            commits.borrow_and_update();
            let db = Arc::clone(&node.db);
            let ran = node
                .rounds
                .call(
                    (session, following),
                    move |(held, mut open): (Detached, Feed)| {
                        let mut attached = held.attach(db.store());
                        let mut texts = Vec::new();
                        let round =
                            open.round(&db, &mut attached, &mut |change, name, allowed, cursor| {
                                // A change whose table has been dropped has no name to give.
                                let Some(table) = name else {
                                    return true;
                                };
                                let names = db
                                    .names_in(&[(
                                        change.id.clone(),
                                        match &change.kind {
                                            tessaridb::ChangeKind::Written(held) => held.clone(),
                                            tessaridb::ChangeKind::Removed => {
                                                tessaridb::Value::Null
                                            }
                                        },
                                    )])
                                    .unwrap_or_default();
                                texts.push(follow::encode(change, table, allowed, &names, cursor));
                                true
                            });
                        ((attached.detach(), open), round, texts)
                    },
                )
                .await;
            let round = match ran {
                Bridged::Answered(((back, open), round, texts)) => {
                    (session, following) = (back, open);
                    due = false;
                    for text in texts {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            // The client has gone; nobody is left to close to.
                            return;
                        }
                    }
                    round
                }
                // Every round slot is taken: stay due and try again after the next
                // wait. A busy node delays a feed; it does not refuse a subscriber
                // already admitted.
                Bridged::Busy((back, open)) => {
                    (session, following) = (back, open);
                    Ok(Round::Empty)
                }
                Bridged::Panicked => return end(&mut socket, 1011).await,
            };
            match round {
                // A mouthful: there may be more, so look again at once.
                Ok(Round::Delivered) => {
                    due = true;
                    continue;
                }
                Ok(Round::Empty | Round::Ended) => {}
                Err(reason) => return refuse_on(&mut socket, &reason.to_string()).await,
            }
        }
        tokio::select! {
            biased;
            incoming = socket.recv() => match incoming {
                // The client closed or the socket failed: the feed ends, with no
                // write needed to learn it.
                None | Some(Err(_)) => return,
                Some(Ok(Message::Close(_))) => return echo_the_close(&mut socket).await,
                // A feed carries nothing in that direction; pings are answered
                // by the protocol layer.
                Some(Ok(_)) => {}
            },
            _ = commits.changed() => due = true,
            () = tokio::time::sleep(feed::PATIENCE_BETWEEN_ROUNDS) => {}
        }
    }
}

/// Tell the client why, then close with the policy code.
async fn refuse_on(socket: &mut WebSocket, reason: &str) {
    // Best effort, as a close always is: the client may already be gone.
    drop(
        socket
            .send(Message::Text(follow::refusal(reason).into()))
            .await,
    );
    end(socket, 1008).await;
}

/// Let the protocol layer send the close it queued in answer to the client's.
///
/// The echo carrying the client's own code goes out on the next read, so the
/// socket is read to its end rather than dropped — dropped, the echo is never
/// sent and a browser reports an error where a clean end happened.
async fn echo_the_close(socket: &mut WebSocket) {
    while let Some(Ok(_)) = socket.recv().await {}
}

/// Close with `code`.
async fn end(socket: &mut WebSocket, code: u16) {
    drop(
        socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: "".into(),
            })))
            .await,
    );
}

/// The close code that names the rule a client's frame broke.
///
/// Told apart by the error's type, never its wording: a close code chosen by
/// matching text changes meaning the day somebody rewords an error.
fn close_code(why: &axum::Error) -> u16 {
    use tungstenite::error::{CapacityError, Error};
    let found = std::error::Error::source(why).and_then(|inner| inner.downcast_ref::<Error>());
    match found {
        Some(Error::Capacity(CapacityError::MessageTooLong { .. })) => 1009,
        Some(Error::Utf8(_)) => 1007,
        _ => 1002,
    }
}
