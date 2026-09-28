//! A busy connection, served on one thread blocking on its own socket.
//!
//! A statement answered from a task crosses three threads: the worker its bytes
//! woke, a blocking-pool thread that runs it, and a worker again to write the
//! answer. The source served every connection on its own thread, which one
//! wake reached, and at one connection the two extra wakes cost a third of a
//! read (Q-839). So a connection whose statement arrives within [`QUIET`] of its
//! last answer is handed — socket and session — to a store thread, which answers
//! it and every statement after it from a blocking read until the client has
//! said nothing for [`QUIET`]. Then the connection goes back to being a task,
//! which is what lets a node hold far more idle connections than threads.
//!
//! Nothing else changes. Every statement takes a slot of the node's bridge for
//! as long as it runs and is refused *busy* when there is none; a statement that
//! panics takes its connection down and gives its slot back; anything that is
//! not a statement — a subscription — goes back to the task that handles it.

use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::net::TcpStream;
use std::time::Duration;

use tessari_serve::Stopping;
use tessari_session::Detached;

use crate::conversation::{self, Answer, BUSY, Conversation};
use crate::error::Result;
use crate::frame;
use crate::message::Request;

/// How long a store thread waits for a connection's next statement before
/// giving the connection back to the runtime.
///
/// Long beside a client answering its last reply and short beside anybody's
/// idea of an idle connection, so a thread is held for a burst and not for a
/// lull. Measured, not guessed: at 2 ms, four hundred busy clients were slow
/// enough to reply — their own scheduling, not the network — that spells kept
/// ending and restarting, and the read p99 was three times the source's; at
/// 20 ms every phase was level with it.
pub(crate) const QUIET: Duration = Duration::from_millis(20);

/// How a busy spell ended.
pub(crate) enum Cooled {
    /// The client fell quiet with nothing unread; the connection goes back to
    /// the runtime with its session.
    Quiet(Detached, TcpStream),
    /// The client sent something that is not a statement. Whatever it sent
    /// after it is dropped: after a subscription that is exactly what the feed
    /// does with a subscriber's bytes, and after any other frame the task ends
    /// the connection.
    Frame(Detached, TcpStream, frame::Kind, Vec<u8>),
    /// The client hung up between frames.
    Closed,
}

/// Answer `first` and every statement that follows it closely.
///
/// Runs on a thread that may block; `stream` is in blocking mode.
///
/// # Errors
///
/// A socket that fails, or a frame that does not decode.
pub(crate) fn serve(
    talk: &Conversation,
    session: Detached,
    stream: TcpStream,
    first: Request,
    theirs: u8,
) -> Result<Cooled> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream.try_clone()?);
    let mut attached = session.attach(talk.db.store());
    let mut request = first;
    loop {
        let answer = match talk.bridge.slot() {
            Some(slot) => {
                let answer =
                    conversation::respond(talk.id, &talk.db, &mut attached, &request, theirs);
                drop(slot);
                answer
            }
            None => {
                log::warn!(
                    "connection {} refused a statement: every store call slot is taken",
                    talk.id
                );
                Answer {
                    kind: frame::Kind::Refusal,
                    body: BUSY.as_bytes().to_vec(),
                }
            }
        };
        reply(&mut writer, &talk.stopping, &answer)?;
        if reader.buffer().is_empty() {
            stream.set_read_timeout(Some(QUIET))?;
            let waited = reader.fill_buf().map(<[u8]>::len);
            stream.set_read_timeout(None)?;
            match waited {
                Ok(0) => return Ok(Cooled::Closed),
                Ok(_) => {}
                Err(why) if matches!(why.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(Cooled::Quiet(attached.detach(), stream));
                }
                Err(why) => return Err(why.into()),
            }
        }
        match frame::read(&mut reader)? {
            None => return Ok(Cooled::Closed),
            Some((frame::Kind::Request, body)) => request = Request::decode(&body)?,
            Some((kind, body)) => return Ok(Cooled::Frame(attached.detach(), stream, kind, body)),
        }
    }
}

/// Write one answer and count it, as the task's `reply` does.
fn reply(writer: &mut impl Write, counting: &Stopping, answer: &Answer) -> Result<()> {
    counting.answered(answer.kind == frame::Kind::Refusal);
    frame::write(writer, answer.kind, &answer.body)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use tessari_serve::{Bridge, Stopping};
    use tessaridb::feed::Commits;
    use tessaridb::{Db, Parameters};
    use tokio::sync::Semaphore;

    use super::{Cooled, serve};
    use crate::conversation::{BUSY, Conversation};
    use crate::error::Result;
    use crate::frame;
    use crate::message::Request;

    const SETUP: &str = "DEFINE NAMESPACE shop; USE NAMESPACE shop; DEFINE DATABASE orders; \
                         USE DATABASE orders; DEFINE COLLECTION items;";

    fn request(script: &str) -> Request {
        Request {
            script: script.to_owned(),
            credentials: None,
            parameters: Parameters::new(),
        }
    }

    /// The bytes of one statement frame.
    fn statement(script: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        frame::write(&mut bytes, frame::Kind::Request, &request(script).encode()).expect("a frame");
        bytes
    }

    /// A busy spell over a real socket, answering `first`.
    ///
    /// `then` is on the socket before the spell starts, so what the spell reads
    /// after its first answer does not depend on how fast the test thread is —
    /// only the silence after it does, and silence is what ends a spell.
    fn spell(
        bridge: Arc<Bridge>,
        first: &str,
        then: &[u8],
    ) -> (TcpStream, mpsc::Receiver<Result<Cooled>>, Arc<Db>) {
        let db = Arc::new(Db::in_memory().expect("an in-memory store"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let mut client =
            TcpStream::connect(listener.local_addr().expect("its address")).expect("a connection");
        let (server, _) = listener.accept().expect("the connection");
        client.write_all(then).expect("what the client says next");
        let talk = Conversation {
            id: 1,
            db: Arc::clone(&db),
            committed: Arc::new(Commits::default()),
            stopping: Stopping::new(),
            bridge,
            rounds: Arc::new(Bridge::new(1)),
            hot: Arc::new(Semaphore::new(1)),
        };
        let session = db.session().detach();
        let first = request(first);
        let (ended, outcome) = mpsc::channel();
        drop(std::thread::spawn(move || {
            drop(ended.send(serve(&talk, session, server, first, frame::MINOR)));
        }));
        (client, outcome, db)
    }

    fn answered(client: &mut TcpStream) -> (frame::Kind, Vec<u8>) {
        frame::read(client)
            .expect("a frame")
            .expect("an answer, not a hang-up")
    }

    fn ended(outcome: &mpsc::Receiver<Result<Cooled>>) -> Result<Cooled> {
        outcome
            .recv_timeout(Duration::from_secs(10))
            .expect("the spell to end")
    }

    #[test]
    fn a_spell_answers_what_follows_and_gives_back_the_session_when_the_client_falls_quiet() {
        let (mut client, outcome, db) = spell(
            Arc::new(Bridge::new(1)),
            SETUP,
            &statement("CREATE items:1 = { n: 1 };"),
        );
        assert_eq!(answered(&mut client).0, frame::Kind::Answer);
        let (kind, body) = answered(&mut client);
        assert_eq!(
            kind,
            frame::Kind::Answer,
            "the statement behind the first was not answered: {}",
            String::from_utf8_lossy(&body)
        );
        let Ok(Cooled::Quiet(back, _)) = ended(&outcome) else {
            panic!("a client that fell quiet was not given back to the runtime");
        };
        // What the first statement selected is still selected: the session came
        // back whole rather than being opened again.
        let mut session = back.attach(db.store());
        let read = session
            .run_with("SELECT * FROM items;", &Parameters::new())
            .expect("the selection the spell made");
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn a_frame_that_is_not_a_statement_goes_back_to_the_task_untouched() {
        let mut subscribe = Vec::new();
        frame::write(&mut subscribe, frame::Kind::Subscribe, b"follow").expect("a frame");
        let (mut client, outcome, _db) = spell(Arc::new(Bridge::new(1)), SETUP, &subscribe);
        assert_eq!(answered(&mut client).0, frame::Kind::Answer);
        let Ok(Cooled::Frame(_, _, kind, body)) = ended(&outcome) else {
            panic!("a subscription was not handed back");
        };
        assert_eq!(kind, frame::Kind::Subscribe);
        assert_eq!(body, b"follow");
    }

    #[test]
    fn bytes_after_a_subscription_are_not_answered_as_statements() {
        // A feed reads a subscriber's bytes and answers none of them; the spell
        // must not answer them either, nor refuse the subscription over them.
        let mut both = Vec::new();
        frame::write(&mut both, frame::Kind::Subscribe, b"follow").expect("a frame");
        both.extend(statement("SELECT * FROM items;"));
        let (mut client, outcome, _db) = spell(Arc::new(Bridge::new(1)), SETUP, &both);
        assert_eq!(answered(&mut client).0, frame::Kind::Answer);
        let Ok(Cooled::Frame(_, _, kind, _)) = ended(&outcome) else {
            panic!("the subscription was answered, refused or lost");
        };
        assert_eq!(kind, frame::Kind::Subscribe);
    }

    #[test]
    fn a_full_bridge_refuses_on_the_thread_as_on_the_task_and_the_spell_goes_on() {
        let bridge = Arc::new(Bridge::new(1));
        let held = bridge.slot().expect("the only slot");
        let (mut client, outcome, _db) = spell(
            Arc::clone(&bridge),
            SETUP,
            &statement("SELECT * FROM items;"),
        );
        for _ in 0..2 {
            let (kind, body) = answered(&mut client);
            assert_eq!(kind, frame::Kind::Refusal, "a full bridge answered");
            assert_eq!(body, BUSY.as_bytes(), "refused, but not for being busy");
        }
        assert_eq!(bridge.refused(), 2, "a refusal went uncounted");
        drop(client);
        let Ok(Cooled::Closed) = ended(&outcome) else {
            panic!("a hang-up was not read as one");
        };
        drop(held);
    }
}
