//! The client: asking, and being told.
//!
//! A [`Client`] asks and reads the answer. A [`Feed`] is what a client becomes
//! when it stops asking — see `push.rs` for why that is a change of type rather
//! than a mode.

use std::io::{BufReader, BufWriter};
use std::net::{TcpStream, ToSocketAddrs};

use crate::error::{Error, Result};
use tessari_ql::Parameters;

use crate::message::{Answer, Request};
use crate::push::{Follow, Happened};
use crate::{frame, message};

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
            let mut both = frame::Duplex {
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
        self.run_with(script, credentials, &Parameters::new())
    }

    /// Run a script whose parameters take the values `parameters` binds.
    ///
    /// The values travel in the store's own codec, so all fifteen kinds cross
    /// unchanged and the server never has to *read* one — which is what keeps
    /// the grammar's rule intact at this distance: a supplied value cannot
    /// become syntax, and nothing about being remote gives that back.
    ///
    /// # Errors
    ///
    /// As [`Client::run`], and [`Error::Refused`] naming the parameter when the
    /// script asks for one this map has no value for.
    pub fn run_with(
        &mut self,
        script: &str,
        credentials: Option<(&str, &str)>,
        parameters: &Parameters,
    ) -> Result<Vec<Answer>> {
        let request = Request {
            script: script.to_owned(),
            credentials: credentials.map(|(name, password)| (name.to_owned(), password.to_owned())),
            parameters: parameters.clone(),
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
            // A node does not send a request, and a change only arrives on a
            // connection that asked to follow — which this one has not.
            frame::Kind::Request | frame::Kind::Subscribe | frame::Kind::Change => {
                Err(Error::UnknownFrame { tag: kind.tag() })
            }
        }
    }

    /// Send a request already built, and hand back the reply frame unread.
    ///
    /// For a node forwarding a write it may not take (ADR-0019 §2, case
    /// *forward*). The bytes are **not** decoded and re-encoded: the answer is
    /// already in the store's own codec, the forwarding node has nothing to add
    /// to it, and a round trip through `Answer` and back would put a second
    /// encoder on the path where the two could disagree about a value neither
    /// node ever looked at.
    ///
    /// A refusal relays too, and relays *as* a refusal — the caller asked the
    /// cluster to run a statement, and the leader's own words about why it would
    /// not are the truthful answer. Nothing here rewrites them to mention the
    /// hop, because a client that mistyped a statement is owed the parser's
    /// message and not a routing story.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] when the peer hung up before replying, and
    /// the stream's failure otherwise.
    #[cfg(feature = "server")]
    pub(crate) fn relay(&mut self, request: &Request) -> Result<(frame::Kind, Vec<u8>)> {
        frame::write(&mut self.writer, frame::Kind::Request, &request.encode())?;
        frame::read(&mut self.reader)?.ok_or(Error::Truncated)
    }

    /// Stop asking, and start being told.
    ///
    /// Consumes the client, because the connection stops being a conversation:
    /// a socket that is delivering changes is not also answering scripts, and a
    /// type that let a caller try would be promising a multiplexing this
    /// protocol does not do. A client that wants both opens two connections.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] carrying the node's own words when the table
    /// is not one it can watch, and the stream's failure otherwise.
    pub fn follow(mut self, asked: &Follow) -> Result<Feed> {
        frame::write(&mut self.writer, frame::Kind::Subscribe, &asked.encode())?;
        Ok(Feed {
            reader: self.reader,
        })
    }
}

/// Changes, as they arrive.
///
/// # It is an iterator of one thing at a time and not a callback
///
/// A subscriber that is busy is *behind*, never lossy — the log is the buffer —
/// so the honest shape is one that lets the caller take the next change when it
/// is ready for it. A callback would invert that and put the node's pace in
/// charge of the subscriber's.
#[derive(Debug)]
pub struct Feed {
    reader: BufReader<TcpStream>,
}

impl Feed {
    /// The next change, or `None` when the node hung up.
    ///
    /// Blocks until there is one. A subscriber that wants to do something else
    /// meanwhile gives this its own thread — which is what the node has done
    /// for it on the other side.
    ///
    /// # Not an `Iterator`
    ///
    /// An `Iterator<Item = Result<Happened>>` would have to fold "the node hung
    /// up" into the same `None` that means "no more items", and those are
    /// different facts to a subscriber holding a position: one is a reason to
    /// reconnect and the other is not. Keeping both in the return type is worth
    /// more than a `for` loop.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Refused`] with the node's words when it refused the
    /// subscription, and the stream's failure otherwise.
    pub fn wait(&mut self) -> Result<Option<Happened>> {
        let Some((kind, body)) = frame::read(&mut self.reader)? else {
            return Ok(None);
        };
        match kind {
            frame::Kind::Change => Ok(Some(Happened::decode(&body)?)),
            // The refusal for a subscription that could not be started arrives
            // here rather than at `follow`, because the node reads the frame
            // before it can judge it.
            frame::Kind::Refusal => Err(Error::Refused {
                message: String::from_utf8(body).unwrap_or_else(|_| "unreadable".to_owned()),
            }),
            frame::Kind::Request | frame::Kind::Answer | frame::Kind::Subscribe => {
                Err(Error::UnknownFrame { tag: kind.tag() })
            }
        }
    }
}
