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
use crate::redirect::Elsewhere;
use crate::{frame, message};

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
                Ok(Served::Answers(answers))
            }
            frame::Kind::Elsewhere => Ok(Served::Elsewhere(Elsewhere::decode(&body)?)),
            // A node does not send a request, and a change only arrives on a
            // connection that asked to follow — which this one has not.
            frame::Kind::Request | frame::Kind::Subscribe | frame::Kind::Change => {
                Err(Error::UnknownFrame { tag: kind.tag() })
            }
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
            // A redirect belongs to a read that can be answered elsewhere. A
            // subscription is a position in one node's log, so there is nothing
            // for another node to answer and this arm stays a refusal even after
            // redirects are followed.
            frame::Kind::Request
            | frame::Kind::Answer
            | frame::Kind::Subscribe
            | frame::Kind::Elsewhere => Err(Error::UnknownFrame { tag: kind.tag() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, BufWriter};
    use std::net::TcpListener;
    use std::thread;

    use tessari_encoding::NODE_ID_LEN;
    use tessari_types::Epoch;

    use super::*;
    use crate::redirect::Settlement;

    /// A node that greets, reads one frame, and answers with the tag it was
    /// given — the smallest thing a real `Client` will talk to over a real
    /// socket, which is the only way to exercise `run_routed`'s reader.
    ///
    /// Bound to port zero rather than a number: the two suites that use fixed
    /// ports have to run alone, and this one has no reason to join them.
    fn node_answering(kind: frame::Kind, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = listener.local_addr().expect("the port it took").to_string();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the client this test dials");
            let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
            let mut writer = BufWriter::new(stream);
            let mut both = frame::Duplex {
                reader: &mut reader,
                writer: &mut writer,
            };
            frame::greet(&mut both).expect("a greeting from a client this build wrote");
            frame::read(&mut reader)
                .expect("the request")
                .expect("a request");
            frame::write(&mut writer, kind, &body).expect("the answer this test exists to send");
        });
        address
    }

    fn a_redirect() -> Elsewhere {
        Elsewhere {
            endpoint: "10.0.0.9:9080".to_owned(),
            node: [3; NODE_ID_LEN],
            epoch: Epoch::new(41),
            settlement: Settlement::Settled,
        }
    }

    #[test]
    fn a_redirect_is_an_answer_to_run_routed_and_not_an_error() {
        let sent = a_redirect();
        let address = node_answering(frame::Kind::Elsewhere, sent.encode());
        let mut client = Client::connect(&address).expect("a node this test started");
        let served = client
            .run_routed("SELECT 1;", None, &Parameters::new())
            .expect("a redirect is not a failure");
        assert_eq!(served, Served::Elsewhere(sent));
    }

    #[test]
    fn a_caller_that_cannot_follow_a_redirect_is_told_where_the_read_belonged() {
        // The point of the wrapper: `run_with` must fail — its contract is
        // *give me the answers* — but the failure has to name the endpoint
        // rather than report tag 13 as an unknown frame, which is what it said
        // before this wave.
        let address = node_answering(frame::Kind::Elsewhere, a_redirect().encode());
        let mut client = Client::connect(&address).expect("a node this test started");
        let held = client
            .run_with("SELECT 1;", None, &Parameters::new())
            .expect_err("a caller asking for answers cannot follow an instruction");
        assert!(
            matches!(held, Error::Redirected { ref endpoint, .. } if endpoint == "10.0.0.9:9080"),
            "the refusal said {held:?} instead of naming where the read belonged"
        );
    }

    #[test]
    fn a_subscriber_still_refuses_a_redirect() {
        // A subscription is a position in ONE node's log, so there is nothing
        // for another node to answer and this refusal outlives the delivery
        // work — it is not a placeholder.
        let address = node_answering(frame::Kind::Elsewhere, a_redirect().encode());
        let client = Client::connect(&address).expect("a node this test started");
        let mut feed = client
            .follow(&Follow {
                from: 0,
                table: None,
            })
            .expect("the subscription this test sends");
        assert!(matches!(feed.wait(), Err(Error::UnknownFrame { tag: 13 })));
    }
}
