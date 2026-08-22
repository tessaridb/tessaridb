//! The wire protocol: framed TCP carrying values in the store's own encoding.
//!
//! # The decision this crate exists to take
//!
//! The HTTP endpoint chose a synchronous server and named where the real
//! decision belonged: *"a thread per concurrent request is right for one node
//! and wrong for ten thousand idle connections, and when that is the problem it
//! belongs to the wire protocol taking the async decision deliberately."*
//!
//! Taken here, and the answer is **synchronous, a thread per connection, no
//! runtime**:
//!
//! - The store below is synchronous by design, because a commit is a
//!   compare-and-set against a substrate. An async server over it would put
//!   `spawn_blocking` at every call, which is a thread pool wearing a runtime's
//!   clothes and a large dependency to wear them.
//! - The cost is a thread per **connection**, and a subscription holds one open —
//!   so the ceiling is subscribers rather than requests. Hundreds is fine. Tens
//!   of thousands is not, and that is the trigger for revisiting this: idle
//!   subscribers outnumbering what a thread each is worth.
//! - It needs **no dependency at all**. `std::net`, length-prefixed frames, and
//!   the encoding crate. For a network-facing surface that is worth more than
//!   the convenience of a framework.
//!
//! # Why not JSON, when there is already an endpoint that speaks it
//!
//! Because JSON has six types and this store has fifteen. The HTTP surface pays
//! that price deliberately — a browser is owed JSON — and quotes a decimal so it
//! is not silently a double. A client reading that back has to *guess*: is
//! `"12.34"` a decimal, and `"2s"` a duration? Here a value goes through the
//! codec the store writes records with, so fifteen types go out and fifteen come
//! back, and neither end decides anything.
//!
//! # What it is not, yet
//!
//! **Subscription push** is W2. The frames are kinded rather than a plain
//! request-and-reply precisely so a server can send something the client did not
//! ask for, and the tags above 3 are reserved for it.
//!
//! **There is no TLS.** A protocol that carries credentials in the clear belongs
//! on a trusted network and nowhere else, and this says so rather than leaving it
//! to be assumed.

#![forbid(unsafe_code)]

mod error;
mod frame;
mod message;

use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;

use bgv_db::Db;

pub use crate::error::{Error, Result};
pub use crate::message::{Answer, Names, Request, names_for, spell};

/// A node listening for connections.
pub struct Node {
    listener: TcpListener,
    db: Arc<Db>,
}

impl Node {
    /// Listen on `address`.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be bound.
    pub fn bind(db: Arc<Db>, address: impl ToSocketAddrs) -> Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(address)?,
            db,
        })
    }

    /// Where it is listening, which a caller needs when it asked for port zero.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the socket cannot say.
    pub fn address(&self) -> Result<String> {
        Ok(self.listener.local_addr()?.to_string())
    }

    /// Serve until the listener fails.
    ///
    /// One thread per connection — see the module documentation for why that is
    /// the decision rather than the shortfall.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let db = Arc::clone(&self.db);
            // A connection that goes wrong takes its own thread down and nothing
            // else: a node that could be stopped by one client's malformed frame
            // would be a node anybody can stop.
            drop(std::thread::spawn(move || drop(converse(&db, stream))));
        }
    }

    /// Serve exactly one connection, for a caller driving the loop itself.
    ///
    /// # Errors
    ///
    /// Returns the failure that ended the conversation.
    pub fn serve_one(&self) -> Result<()> {
        let (stream, _) = self.listener.accept()?;
        converse(&self.db, stream)
    }
}

/// One connection, from hello to hang-up.
///
/// # One session, not one per statement
///
/// The session is opened once and lives as long as the connection, because that
/// is what a connection *is*: `USE NAMESPACE prod;` selects something, and a
/// selection that does not survive to the next statement is not a selection. A
/// session per request would make a prompt over this protocol a sequence of
/// unrelated sessions that happen to share a socket, and every statement would
/// have to re-say where it was.
///
/// It also gives the thread-per-connection cost something to buy. A thread here
/// holds state a request cannot carry, which is the difference between a
/// connection and a datagram.
fn converse(db: &Db, stream: TcpStream) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    {
        // The greeting needs both directions on one object; after it they are
        // used independently, which is what lets a push frame be written while a
        // read is waiting.
        let mut both = Duplex {
            reader: &mut reader,
            writer: &mut writer,
        };
        frame::greet(&mut both)?;
    }

    let mut session = db.session();
    while let Some((kind, body)) = frame::read(&mut reader)? {
        if kind != frame::Kind::Request {
            // A client sending an answer is a client this build does not
            // understand, and continuing would be guessing at what it meant.
            return Err(Error::UnknownFrame { tag: kind.tag() });
        }
        let request = Request::decode(&body)?;
        if let Some((name, password)) = &request.credentials
            && let Err(refusal) = session.sign_in(name, password)
        {
            // The session's own refusal, travelling as one. A second rule here
            // would be a second place for "who may do this" to be decided.
            frame::write(
                &mut writer,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )?;
            continue;
        }
        match session.run(&request.script) {
            Ok(outcomes) => {
                let mut answer = Vec::new();
                frame::put_u32(
                    &mut answer,
                    u32::try_from(outcomes.len()).unwrap_or(u32::MAX),
                );
                for outcome in &outcomes {
                    // Resolved here because the catalog is here. `names_in`
                    // walks the answer first and touches nothing when it holds
                    // no reference, which is most answers.
                    let names = message::names_for(db, outcome);
                    answer.extend_from_slice(&message::encode_outcome(outcome, &names));
                }
                frame::write(&mut writer, frame::Kind::Answer, &answer)?;
            }
            // A refusal does not close the connection: a client that mistyped a
            // statement has not stopped being a client.
            Err(refusal) => frame::write(
                &mut writer,
                frame::Kind::Refusal,
                refusal.to_string().as_bytes(),
            )?,
        }
    }
    Ok(())
}

/// A reader and a writer over one socket, for the greeting.
struct Duplex<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
}

impl<R: std::io::Read, W: std::io::Write> std::io::Read for Duplex<'_, R, W> {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(into)
    }
}

impl<R: std::io::Read, W: std::io::Write> std::io::Write for Duplex<'_, R, W> {
    fn write(&mut self, from: &[u8]) -> std::io::Result<usize> {
        self.writer.write(from)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// A connection to a node.
///
/// `Debug` says where it is connected and nothing about the buffers, because
/// what is worth printing about a connection is the other end of it.
pub struct Client {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field(
                "peer",
                &self
                    .writer
                    .get_ref()
                    .peer_addr()
                    .map_or_else(|_| "disconnected".to_owned(), |held| held.to_string()),
            )
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect and exchange greetings.
    ///
    /// # Errors
    ///
    /// Returns the operating system's failure when the address cannot be
    /// reached, and [`Error::NotThisProtocol`] or [`Error::WrongVersion`] when
    /// whatever answered is not a node this build speaks to.
    pub fn connect(address: impl ToSocketAddrs) -> Result<Self> {
        let stream = TcpStream::connect(address)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = BufWriter::new(stream);
        {
            let mut both = Duplex {
                reader: &mut reader,
                writer: &mut writer,
            };
            frame::greet(&mut both)?;
        }
        Ok(Self { reader, writer })
    }

    /// Run a script and read what came back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] carrying the store's own message when the
    /// store refused, and the stream's failure otherwise.
    pub fn run(&mut self, script: &str, credentials: Option<(&str, &str)>) -> Result<Vec<Answer>> {
        let request = Request {
            script: script.to_owned(),
            credentials: credentials.map(|(name, password)| (name.to_owned(), password.to_owned())),
        };
        frame::write(&mut self.writer, frame::Kind::Request, &request.encode())?;

        let Some((kind, body)) = frame::read(&mut self.reader)? else {
            return Err(Error::Truncated);
        };
        match kind {
            frame::Kind::Refusal => Err(Error::Refused {
                message: String::from_utf8(body).unwrap_or_else(|_| "unreadable".to_owned()),
            }),
            frame::Kind::Answer => {
                let (count, mut at) = frame::take_u32(&body, 0)?;
                let mut answers = Vec::new();
                for _ in 0..count {
                    let (answer, next) = message::decode_outcome(&body, at)?;
                    at = next;
                    answers.push(answer);
                }
                Ok(answers)
            }
            frame::Kind::Request => Err(Error::UnknownFrame { tag: kind.tag() }),
        }
    }
}
