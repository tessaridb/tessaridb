//! The client: asking, and being told.
//!
//! A [`Client`] asks and reads the answer. A [`Feed`] is what a client becomes
//! when it stops asking — see `push.rs` for why that is a change of type rather
//! than a mode.

use std::io::{BufReader, BufWriter};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(feature = "tls")]
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use tessari_ql::Parameters;

use crate::message::{Answer, Request};
use crate::push::{Follow, Happened};
use crate::redirect::Elsewhere;
use crate::transport::Transport;
use crate::{frame, message};
use tessari_types::Epoch;

/// What a node did with a script: answered it, or said where it belongs.
///
/// Two answers rather than an `Option` or an error, for the reason
/// [`crate::Destination`] gives one layer down — *answered here* and *go to that
/// node* are both successes and mean opposite things to the caller. An
/// instruction handed back as a failure is followed by nobody, because every
/// client that treats failures correctly treats this one incorrectly.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Served {
    /// The node answered, one outcome per statement.
    Answers(Vec<Answer>),
    /// The node did not answer, and this is where the read belongs.
    Elsewhere(Elsewhere),
}

/// A connection to a node.
///
/// `Debug` says where it is connected and nothing about the buffers, because
/// what is worth printing about a connection is the other end of it.
pub struct Client {
    reader: BufReader<Transport>,
    writer: BufWriter<Transport>,
    /// The newest leadership any node has named in a redirect to this client.
    ///
    /// `None` until the first one arrives — a client that has been told nothing
    /// has nothing to compare against, and every redirect is news to it.
    ///
    /// This is the whole of the client's routing memory, and it is deliberately
    /// one number rather than a chain: what a caller needs to distinguish is a
    /// loop from progress, and the epoch already answers that. A hop counter or
    /// a redirect history would be a second mechanism for a job this does.
    seen: Option<Epoch>,
    /// The minor version the node said at the greeting, which decides what this
    /// client may send it.
    minor: u8,
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
        let socket = TcpStream::connect(address)?;
        socket.set_nodelay(true)?;
        Self::greeted(Transport::Plain(socket))
    }

    /// Connect over TLS, trusting `roots`, and exchange greetings.
    ///
    /// The certificate must carry the host part of `address` — a name or an
    /// IP address — and chain to one of `roots`. There is no way to skip
    /// either check: a client that would accept any certificate is a client
    /// talking to whoever answered.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`], and [`Error::Tls`] when the handshake fails.
    #[cfg(feature = "tls")]
    pub fn connect_tls(address: &str, roots: rustls::RootCertStore) -> Result<Self> {
        let name = crate::transport::server_name(address)?;
        let settings =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_root_certificates(roots)
                .with_no_client_auth();
        let mut session = rustls::ClientConnection::new(Arc::new(settings), name)
            .map_err(|why| Error::Tls(why.to_string()))?;
        let mut socket = TcpStream::connect(address)?;
        socket.set_nodelay(true)?;
        // Completed here rather than on the first write, so a certificate the
        // client will not trust is reported as that and not as a greeting
        // that never came back.
        while session.is_handshaking() {
            session
                .complete_io(&mut socket)
                .map_err(|why| Error::Tls(why.to_string()))?;
        }
        Self::greeted(Transport::Tls(Arc::new(Mutex::new(
            rustls::StreamOwned::new(session, socket),
        ))))
    }

    /// Greet over a connection that is already open.
    fn greeted(stream: Transport) -> Result<Self> {
        let mut reader = BufReader::new(stream.duplicate()?);
        let mut writer = BufWriter::new(stream);
        let minor = {
            let mut both = frame::Duplex {
                reader: &mut reader,
                writer: &mut writer,
            };
            frame::greet(&mut both)?
        };
        Ok(Self {
            reader,
            writer,
            seen: None,
            minor,
        })
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

    /// Unseal, seal or ask about the vault, with the passphrase as a field of
    /// the frame rather than script text (ADR-0092 D2), on the store's key or,
    /// with `place`, on one vault carrying its own passphrase (ADR-0093 D6).
    ///
    /// Answers the seal status as one value.
    ///
    /// # Errors
    ///
    /// [`Error::NodeTooOld`] before anything is sent to a node below minor 2;
    /// [`Error::Refused`] with the store's own message when the node refused —
    /// which never quotes the passphrase; the stream's failure otherwise.
    pub fn vault(
        &mut self,
        call: &crate::VaultCall,
        place: Option<&crate::VaultPlace>,
        credentials: Option<(&str, &str)>,
    ) -> Result<Answer> {
        if self.minor < frame::VAULT {
            return Err(Error::NodeTooOld {
                found: self.minor,
                needed: frame::VAULT,
            });
        }
        let asked = crate::VaultAsk {
            call: call.clone(),
            credentials: credentials.map(|(name, password)| (name.to_owned(), password.to_owned())),
            place: place.cloned(),
        };
        frame::write(&mut self.writer, frame::Kind::Vault, &asked.encode())?;
        let Some((kind, body)) = frame::read(&mut self.reader)? else {
            return Err(Error::Truncated);
        };
        match kind {
            frame::Kind::Refusal => Err(refused(&body)),
            frame::Kind::Answer => {
                let (count, at) = frame::take_u32(&body, 0)?;
                if count != 1 {
                    return Err(Error::Malformed);
                }
                Ok(message::decode_outcome(&body, at)?.0)
            }
            other => Err(Error::UnknownFrame { tag: other.tag() }),
        }
    }

    /// Run a script and take back either the answers or the redirect.
    ///
    /// The primitive [`Self::run`] and [`Self::run_with`] are written on top of.
    /// A node may decline a read because a copy elsewhere is the one inside the
    /// staleness bound the caller asked for, and *answered here* and *go to that
    /// node* are both successes that mean opposite things — the same argument
    /// [`crate::Destination`] makes one layer down, where the decision is taken.
    ///
    /// A caller that cannot act on a redirect wants `run_with`, which turns it
    /// into a refusal naming where the read belonged.
    ///
    /// # Errors
    ///
    /// As [`Self::run_with`], except that a redirect is an answer here rather
    /// than an error.
    pub fn run_routed(
        &mut self,
        script: &str,
        credentials: Option<(&str, &str)>,
        parameters: &Parameters,
    ) -> Result<Served> {
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
            frame::Kind::Refusal => Err(refused(&body)),
            frame::Kind::Answer => {
                let (count, mut at) = frame::take_u32(&body, 0)?;
                let mut answers = Vec::new();
                for _ in 0..count {
                    let (answer, next) = message::decode_outcome(&body, at)?;
                    at = next;
                    answers.push(answer);
                }
                Ok(Served::Answers(answers))
            }
            frame::Kind::Elsewhere => {
                let sent = Elsewhere::decode(&body)?;
                // Strictly older, never merely not-newer. Two redirects under
                // one leadership are the ordinary two-hop route — sent to one
                // node, and that node sending this caller to a second — and
                // refusing equality would break it.
                if self.seen.is_some_and(|held| sent.epoch < held) {
                    return Err(Error::StaleRedirect {
                        named: sent.epoch,
                        // Unwrapped against the guard immediately above: the
                        // comparison only ran because there was something to
                        // compare against.
                        held: self.seen.unwrap_or(sent.epoch),
                    });
                }
                self.seen = Some(sent.epoch);
                Ok(Served::Elsewhere(sent))
            }
            // A node does not send a request, and a change only arrives on a
            // connection that asked to follow — which this one has not.
            frame::Kind::Request
            | frame::Kind::Subscribe
            | frame::Kind::Change
            | frame::Kind::Vault => Err(Error::UnknownFrame { tag: kind.tag() }),
        }
    }

    /// Run a script whose parameters take the values `parameters` binds.
    ///
    /// The values travel in the store's own codec, so all seventeen kinds cross
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
        match self.run_routed(script, credentials, parameters)? {
            Served::Answers(answers) => Ok(answers),
            // Not the frame's shape leaking back as an error. This method's
            // contract is *give me the answers*, and a caller that asked for
            // answers, cannot follow an instruction, and was handed one has
            // genuinely failed. What matters is that the refusal names where the
            // read belonged — [`Self::run_routed`] is where a caller that can act
            // on it gets it intact.
            Served::Elsewhere(elsewhere) => Err(Error::Redirected {
                endpoint: elsewhere.endpoint,
                node: elsewhere.node,
            }),
        }
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
    reader: BufReader<Transport>,
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
            frame::Kind::Refusal => Err(refused(&body)),
            // A redirect belongs to a read that can be answered elsewhere. A
            // subscription is a position in one node's log, so there is nothing
            // for another node to answer and this arm stays a refusal even after
            // redirects are followed.
            frame::Kind::Request
            | frame::Kind::Answer
            | frame::Kind::Subscribe
            | frame::Kind::Elsewhere
            | frame::Kind::Vault => Err(Error::UnknownFrame { tag: kind.tag() }),
        }
    }
}

#[cfg(test)]
mod tests;

/// A refusal frame's body as the error a caller matches on.
fn refused(body: &[u8]) -> Error {
    let (class, message) = frame::read_refusal(body);
    Error::Refused { message, class }
}
