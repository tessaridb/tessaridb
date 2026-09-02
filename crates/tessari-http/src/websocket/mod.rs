//! `GET /watch` — the route a browser upgrades on.
//!
//! ADR-0016 decided one route where `GET` upgrades to a WebSocket carrying the
//! change feed, and that the server half of RFC 6455 is written here rather than
//! taken as a dependency. This module is the protocol; the feed it will carry is
//! SGK.T3.
//!
//! # What this route does today
//!
//! It completes the handshake and speaks the control protocol: a ping is
//! answered, a close is echoed, a fragmented message is reassembled, and a frame
//! that breaks the rules ends the connection with the code that says which rule.
//! A **data** message has nothing to consume it yet, so it is answered with close
//! `1003` — a stated refusal rather than silence, and one arm for SGK.T3 to
//! replace.
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

mod follow;
mod frame;
mod handshake;
mod sha1;

use std::io::{Read, Write};

use tessari_constants::SOCKET_MAX_MESSAGE_BYTES;
use tessari_serve::{Busy, Stopping};
use tessaridb::feed::{Commits, Following};
use tessaridb::{Db, Sequence};
use tiny_http::{Header, Request, Response};

use crate::basic::{self, Credentials, Presented};
use crate::tokens::Tokens;
use frame::{Frame, Opcode};

/// Serve one request to the socket route.
///
/// Returns whether the request was refused, which is what the surface counts as
/// a refusal — the same definition every other route here uses.
pub(crate) fn watch(
    db: &Db,
    stopping: &Stopping,
    committed: &Commits,
    tokens: &Tokens,
    request: Request,
    busy: &mut Busy,
) -> bool {
    // Read before the upgrade, because the upgrade consumes the request. A
    // browser cannot set this header, which is why a follow request may carry a
    // credential of its own — see `follow.rs`.
    let presented = basic::presented(
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Authorization"))
            .map(|header| header.value.as_str()),
    );
    let accept = match handshake::read(request.headers()) {
        Ok(accept) => accept,
        Err(refusal) => {
            let (status, body) = refusal.answer();
            return refuse(request, status, body);
        }
    };
    // The value is base64 of a digest, so it is header-safe by construction; a
    // failure here would be a bug in this file rather than something a client
    // did, and answering `500` says so instead of sending a `101` a client
    // cannot verify.
    let Ok(header) = Header::from_bytes(&b"Sec-WebSocket-Accept"[..], accept.as_bytes()) else {
        return refuse(
            request,
            500,
            r#"{"error":"the upgrade could not be answered"}"#,
        );
    };

    // Before the upgrade rather than after: between the two the connection is
    // neither, and a drain sampling in that gap must see the safer of the two
    // answers.
    busy.became_a_feed();
    let mut socket = request.upgrade("websocket", Response::empty(101).with_header(header));
    session(&mut socket, db, stopping, committed, tokens, &presented);
    false
}

/// Answer a request that reached this route without asking to upgrade.
fn refuse(request: Request, status: u16, body: &str) -> bool {
    let mut response = tiny_http::Response::from_string(body).with_status_code(status);
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
        response = response.with_header(header);
    }
    // A client that hung up is not this node's problem, and there is nobody left
    // to tell.
    drop(request.respond(response));
    true
}

/// Read frames until the connection ends, answering control frames as they come.
fn session(
    socket: &mut (impl Read + Write),
    db: &Db,
    stopping: &Stopping,
    committed: &Commits,
    tokens: &Tokens,
    presented: &Presented,
) {
    // The bytes reassembled so far, and the kind the FIRST fragment declared —
    // which is now read, because a follow request is text and a binary message
    // is not something this route accepts. Wave 53 deliberately dropped this
    // field for want of a reader; this is the reader.
    let mut message: Option<(Opcode, Vec<u8>)> = None;
    loop {
        let Frame {
            fin,
            opcode,
            payload,
        } = match frame::read(socket) {
            Ok(frame) => frame,
            Err(why) => return end(socket, why.code()),
        };

        // A control frame is never a fragment and may arrive **between** the
        // fragments of a message, which is why it is handled before the
        // reassembly and does not disturb it. A loop written as "read until FIN"
        // gets this wrong and compiles.
        if opcode.is_control() {
            if opcode == Opcode::Close {
                return end(socket, Some(frame::closed_with(&payload)));
            }
            // The pong carries the ping's payload unchanged — RFC 6455 §5.5.3,
            // and an intermediary checks it. An unsolicited pong is explicitly
            // allowed and needs no answer, which is the remaining case.
            if opcode == Opcode::Ping && frame::write(socket, true, Opcode::Pong, &payload).is_err()
            {
                return;
            }
            continue;
        }

        let continued = opcode == Opcode::Continuation;
        match (message.as_mut(), continued) {
            // A continuation with nothing to continue, or a new message while
            // one is still open. Both are protocol faults rather than something
            // to guess at.
            (None, true) | (Some(_), false) => return end(socket, Some(1002)),
            (Some((_, held)), true) => held.extend_from_slice(&payload),
            (None, false) => message = Some((opcode, payload)),
        }

        // Bounded across fragments, not per frame: a sender that fragments
        // without limit gets past a per-frame ceiling by construction.
        if message
            .as_ref()
            .is_some_and(|(_, held)| held.len() > SOCKET_MAX_MESSAGE_BYTES)
        {
            return end(socket, Some(1009));
        }

        if fin {
            let Some((kind, held)) = message else {
                return end(socket, Some(1002));
            };
            // A follow request is text. A binary message is refused with the
            // code that says which kind was wrong rather than a generic fault,
            // because that is the difference between a client fixing its
            // encoding and a client guessing.
            if kind != Opcode::Text {
                return end(socket, Some(1003));
            }
            let Ok(body) = String::from_utf8(held) else {
                // 1007 is "not the data I said it was" — a text frame whose
                // bytes are not text.
                return end(socket, Some(1007));
            };
            // Following takes the connection over: a thread that is pushing
            // cannot also be reading requests, and letting it do both means
            // multiplexing — a much larger protocol for a case nobody has. A
            // client that wants both opens two sockets.
            return feed(socket, db, stopping, committed, presented, tokens, &body);
        }
    }
}

/// Follow changes on this socket until the client goes or the node stops.
///
/// The refusals are written **as a frame** before the close rather than only as
/// a close code: a close code is five bits of meaning, and "you are not granted
/// that table" and "that table does not exist" are different things a subscriber
/// must be able to tell apart.
fn feed(
    socket: &mut (impl Read + Write),
    db: &Db,
    stopping: &Stopping,
    committed: &Commits,
    presented: &Presented,
    tokens: &Tokens,
    body: &str,
) {
    let asked = match follow::read(body) {
        Ok(asked) => asked,
        Err(reason) => return refuse_on(socket, &reason),
    };
    // The header's claim when there was one, else the message's. The header
    // wins, because a client that can set one is not a browser and its
    // credential did not travel through a message body.
    //
    // Within the message a token beats a password, for the same reason the rest
    // of this surface prefers one: it expires, it can be handed back, and a
    // browser holding it is not holding a password.
    let claimed = match presented {
        Presented::Nobody => match (asked.token.clone(), asked.credentials.clone()) {
            (Some(bearer), _) => Presented::Token(bearer),
            (None, Some((name, password))) => Presented::Password(Credentials { name, password }),
            (None, None) => Presented::Nobody,
        },
        held => held.clone(),
    };
    // The same function every other route uses, rather than a second sign-in
    // written here: a subscription reads records, and a feed that authenticated
    // itself differently from the script route is how this surface grew two
    // authorization holes once already.
    let mut session = match crate::respond::session_for(db, tokens, &claimed) {
        Ok(session) => session,
        Err(answer) => return refuse_on(socket, &String::from_utf8_lossy(&answer.body)),
    };
    // A namespace and database reach a statement, so they are guarded by the
    // same narrow rule the object routes use rather than by a second opinion
    // about what the lexer would accept.
    if !crate::object::is_identifier(&asked.namespace)
        || !crate::object::is_identifier(&asked.database)
    {
        return refuse_on(socket, "a namespace and database are plain names");
    }
    let selecting = format!(
        "USE NAMESPACE {} DATABASE {};",
        asked.namespace, asked.database
    );
    if let Err(error) = session.run(&selecting) {
        return refuse_on(socket, &error.to_string());
    }

    let following = Following {
        from: Sequence::new(asked.from),
        table: asked.table.as_deref(),
    };
    let mut broken = false;
    let outcome = tessaridb::feed::follow(
        db,
        &mut session,
        &following,
        committed,
        &|| stopping.asked(),
        &mut |change, name, allowed| {
            let Some(table) = name else {
                // A change whose table has been dropped has no name to give.
                return true;
            };
            // Only reaches the store when the value actually holds a reference,
            // and a client that receives `"1:2"` cannot follow it.
            let names = db
                .names_in(&[(
                    change.id.clone(),
                    match &change.kind {
                        tessaridb::ChangeKind::Written(held) => held.clone(),
                        tessaridb::ChangeKind::Removed => tessaridb::Value::Null,
                    },
                )])
                .unwrap_or_default();
            let text = follow::encode(change, table, allowed, &names);
            if frame::write(socket, true, Opcode::Text, text.as_bytes()).is_err() {
                broken = true;
                return follow::GONE;
            }
            true
        },
    );
    if broken {
        return;
    }
    if let Err(reason) = outcome {
        return refuse_on(socket, &reason);
    }
    // The node is stopping. A subscriber loses nothing: the cursor is a position
    // it holds, so it resumes exactly where it stopped.
    end(socket, Some(1001));
}

/// Tell a subscriber why it is not following anything, then end the connection.
fn refuse_on(socket: &mut (impl Read + Write), reason: &str) {
    let _ = frame::write(
        socket,
        true,
        Opcode::Text,
        follow::refusal(reason).as_bytes(),
    );
    end(socket, Some(1008));
}

/// End the connection, telling the client why when there is one left to tell.
fn end(socket: &mut impl Write, code: Option<u16>) {
    if let Some(code) = code {
        // A close that cannot be written means the client is already gone, which
        // is the outcome the close was announcing.
        let _ = frame::close(socket, code);
    }
}

#[cfg(test)]
mod tests {
    use super::frame::{Opcode, closed_with, write};
    use super::session;

    /// A client's side of a socket: what it sends, and what it reads back.
    struct Client {
        sending: std::io::Cursor<Vec<u8>>,
        received: Vec<u8>,
    }

    impl std::io::Read for Client {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.sending.read(buffer)
        }
    }

    impl std::io::Write for Client {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.received.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Frame `payload` the way a **client** must: masked.
    fn client_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        let mut out = vec![if fin { 0b1000_0000 | opcode } else { opcode }];
        out.push(0b1000_0000 | u8::try_from(payload.len()).unwrap_or(0));
        out.extend_from_slice(&mask);
        out.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        out
    }

    /// Run a session over everything the client sends, and read what came back.
    ///
    /// A real store, because the session's job now ends in one. Nothing here
    /// reaches it: every exchange below is decided by the frame protocol before
    /// a follow request is ever parsed.
    fn exchange(sent: Vec<u8>) -> Vec<(bool, u8, Vec<u8>)> {
        let db = tessaridb::Db::in_memory().expect("an in-memory store");
        let stopping = tessari_serve::Stopping::new();
        let committed = super::Commits::default();
        let mut client = Client {
            sending: std::io::Cursor::new(sent),
            received: Vec::new(),
        };
        let tokens = crate::tokens::Tokens::default();
        session(
            &mut client,
            &db,
            &stopping,
            &committed,
            &tokens,
            &crate::basic::Presented::Nobody,
        );
        unframe(&client.received)
    }

    /// Take apart the **unmasked** frames a server sends.
    fn unframe(bytes: &[u8]) -> Vec<(bool, u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at.saturating_add(2) <= bytes.len() {
            let fin = bytes[at] & 0b1000_0000 != 0;
            let opcode = bytes[at] & 0b0000_1111;
            let length = usize::from(bytes[at.saturating_add(1)] & 0b0111_1111);
            let from = at.saturating_add(2);
            let to = from.saturating_add(length);
            if to > bytes.len() {
                break;
            }
            out.push((fin, opcode, bytes[from..to].to_vec()));
            at = to;
        }
        out
    }

    #[test]
    fn a_ping_is_answered_with_a_pong_carrying_the_same_payload() {
        let answers = exchange(client_frame(true, 9, b"are you there"));
        assert_eq!(
            answers
                .first()
                .map(|(fin, opcode, payload)| (*fin, *opcode, payload.clone())),
            Some((true, 10, b"are you there".to_vec())),
            "a ping went unanswered, or the pong did not carry the ping's payload"
        );
    }

    #[test]
    fn a_close_is_echoed_with_the_code_the_client_sent() {
        let mut sent = client_frame(true, 8, &1001u16.to_be_bytes());
        // Anything after a close must not be read: the connection is over.
        sent.extend(client_frame(true, 9, b"late"));
        let answers = exchange(sent);
        // The echo is checked before the count, because a count is the cheaper
        // assertion and would take the failure for both claims — leaving the one
        // underneath it never actually tested.
        let (_, opcode, payload) = answers
            .first()
            .expect("a close was not echoed at all, so the client is left waiting");
        assert_eq!(*opcode, 8, "a close was not echoed with a close");
        assert_eq!(
            closed_with(payload),
            1001,
            "the echo did not carry the code the client sent, so the client \
             reports an error where a clean end happened"
        );
        assert_eq!(answers.len(), 1, "the session answered after a close");
    }

    #[test]
    fn a_message_split_across_fragments_is_reassembled_with_a_ping_between_them() {
        // The case a "read until FIN" loop gets wrong: a control frame arriving
        // between two fragments of one message.
        let mut sent = client_frame(false, 1, b"{\"follow\":");
        sent.extend(client_frame(true, 9, b"still there"));
        sent.extend(client_frame(true, 0, b"{\"from\":0}}"));
        let answers = exchange(sent);
        assert_eq!(
            answers[0].1, 10,
            "the ping between fragments was not answered"
        );
        // The two halves make `{"follow":{"from":0}}`, which is a well-formed
        // JSON object and *not* a follow request — so what proves the
        // reassembly happened is that the refusal names the field the joined
        // text contains. Either fragment alone would not parse at all, and the
        // wrong join would name something else.
        assert_eq!(
            answers[1].1, 1,
            "the reassembled message was not answered in words"
        );
        let text = String::from_utf8(answers[1].2.clone()).expect("a text frame carries text");
        assert!(
            text.contains("follow"),
            "the refusal does not quote the field the joined message held, so \
             the two fragments were not reassembled into one message: {text}"
        );
        assert_eq!(
            answers[2].1, 8,
            "the connection was left open after the request was refused"
        );
        assert_eq!(
            closed_with(&answers[2].2),
            1008,
            "a request this route understood and rejected is a policy refusal"
        );
    }

    #[test]
    fn a_continuation_with_nothing_to_continue_is_a_protocol_error() {
        let answers = exchange(client_frame(true, 0, b"orphan"));
        assert_eq!(
            answers.first().map(|(_, _, payload)| closed_with(payload)),
            Some(1002),
            "a continuation frame outside a message was accepted"
        );
    }

    #[test]
    fn a_new_message_before_the_last_one_finished_is_a_protocol_error() {
        let mut sent = client_frame(false, 1, b"first");
        sent.extend(client_frame(true, 1, b"second"));
        let answers = exchange(sent);
        assert_eq!(
            answers.first().map(|(_, _, payload)| closed_with(payload)),
            Some(1002),
            "two overlapping messages were accepted, so neither is what arrives"
        );
    }

    #[test]
    fn an_unmasked_client_frame_ends_the_connection_with_the_code_that_says_why() {
        let unmasked = [0b1000_0001u8, 2, b'h', b'i'];
        let answers = exchange(unmasked.to_vec());
        assert_eq!(
            answers.first().map(|(_, _, payload)| closed_with(payload)),
            Some(1002),
            "an unmasked client frame was accepted"
        );
    }

    #[test]
    fn a_client_that_vanishes_is_told_nothing() {
        assert!(
            exchange(Vec::new()).is_empty(),
            "a close was written to a socket whose other end had already gone"
        );
    }

    #[test]
    fn the_server_never_masks_what_it_writes() {
        let mut out = Vec::new();
        write(&mut out, true, Opcode::Pong, b"x").expect("a writable buffer");
        assert_eq!(
            out.get(1).map(|byte| byte & 0b1000_0000),
            Some(0),
            "a masked server frame is refused by every browser"
        );
    }
}
