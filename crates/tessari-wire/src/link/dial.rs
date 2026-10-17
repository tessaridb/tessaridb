use super::*;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio_rustls::TlsConnector;

use crate::frame_async;

/// One dialled connection to a peer: TLS over a socket of the runtime's.
pub(crate) type Link = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Reach the peer `at` on `address`, and exchange greetings.
///
/// # Errors
///
/// Returns [`Error::Transport`] when the node reached does not hold a peer
/// credential for `at` — the handshake refuses it, which is why this side needs
/// no admission rule of its own; [`Error::Uncollectable`] when a collection was
/// asked for from a position the peer cannot state a predecessor for; and
/// [`Error::OutOfTurn`] when the answer is not of the kind that was asked for.
pub async fn call(
    address: impl ToSocketAddrs,
    keys: &PeerKeys,
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Ask<'_>,
) -> Result<(Hello, Answered)> {
    call_within(
        address,
        (keys, keys.duplicate()),
        at,
        said,
        asking,
        Duration::from_secs(GREETING_SECONDS),
    )
    .await
}

/// [`call`], with the connect, the handshake and every read and write bounded
/// by `bound` rather than by the greeting's deadline.
///
/// For a caller that has a deadline of its own — a ballot is worth nothing once
/// its round is over, and a member that accepts the connection and then says
/// nothing must cost the round no more than the round (G053 SG2b).
///
/// `mine` is the credential this dial presents, taken from `keys` by the
/// caller, so a caller that signs with the key as well signs and presents one
/// snapshot even when a rotation lands between the two.
///
/// # Errors
///
/// As [`call`], plus [`Error::Io`] when `bound` passes on any one step.
pub async fn call_within(
    address: impl ToSocketAddrs,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Ask<'_>,
    bound: Duration,
) -> Result<(Hello, Answered)> {
    let mut link = open_within(address, (keys, mine), at, bound).await?;
    let exchanged = exchange(&mut link, said, asking, bound).await;

    // Say goodbye properly even when the exchange failed. A TLS peer that just
    // drops the socket makes the other end's next read an error rather than an
    // end, so a caller that skipped this would leave every door it spoke to
    // reporting a fault it did not have.
    drop(within(bound, link.shutdown()).await);
    exchanged
}

/// Reach the peer `at` on `address` and open the TLS session a conversation
/// rides, with the connect and the handshake bounded by the greeting's deadline.
pub(crate) async fn open(
    address: impl ToSocketAddrs,
    keys: &PeerKeys,
    at: [u8; NODE_ID_LEN],
) -> Result<Link> {
    open_within(
        address,
        (keys, keys.duplicate()),
        at,
        Duration::from_secs(GREETING_SECONDS),
    )
    .await
}

/// [`open`], with the connect and the handshake each bounded by `bound`.
pub(crate) async fn open_within(
    address: impl ToSocketAddrs,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    bound: Duration,
) -> Result<Link> {
    let settings = keys.dialling(mine)?;
    let expected = credential::names(at, Purpose::Peer);
    let name = ServerName::try_from(expected).map_err(|why| Error::Transport(why.to_string()))?;
    let socket = connect(address, bound).await?;
    // A handshake that fails is the transport refusing, kept apart from one
    // that did not finish in time.
    let handshake = TlsConnector::from(Arc::new(settings)).connect(name, socket);
    tokio::time::timeout(bound.max(Duration::from_millis(1)), handshake)
        .await
        .map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))?
        .map_err(|why| Error::Transport(why.to_string()))
}

/// Open the connection a call rides, giving up after `bound` per address.
///
/// The reads and writes were always bounded and the connect was not, so a peer
/// whose host drops SYNs held the calling round for the kernel's own connect
/// limit (Q-836). Each resolved address is tried in turn.
pub(super) async fn connect(address: impl ToSocketAddrs, bound: Duration) -> Result<TcpStream> {
    let mut failed = None;
    for at in tokio::net::lookup_host(address).await? {
        match within(bound, TcpStream::connect(at)).await {
            Ok(socket) => {
                socket.set_nodelay(true)?;
                return Ok(socket);
            }
            Err(why) => failed = Some(why),
        }
    }
    Err(failed.unwrap_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the peer's address resolved to nothing",
        ))
    }))
}

/// `step`, or [`std::io::ErrorKind::TimedOut`] once `bound` has passed.
///
/// A zero would mean the step may not even start, so the least a step may have
/// is a millisecond, as a socket timeout of zero would have meant none at all.
pub(crate) async fn within<T>(
    bound: Duration,
    step: impl Future<Output = std::io::Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(bound.max(Duration::from_millis(1)), step).await {
        Ok(done) => Ok(done?),
        Err(_) => Err(Error::Io(std::io::ErrorKind::TimedOut.into())),
    }
}

/// A frame read or written within `bound`, its own failure kept as it was.
pub(crate) async fn framed<T>(bound: Duration, step: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(bound.max(Duration::from_millis(1)), step)
        .await
        .map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))?
}

/// The greeting and the one follow-up, on a session that is already open.
pub(super) async fn exchange(
    link: &mut Link,
    said: &Hello,
    asking: Ask<'_>,
    bound: Duration,
) -> Result<(Hello, Answered)> {
    framed(bound, say(link, said)).await?;
    // The ask goes out with the greeting rather than a round trip after it: it
    // depends on nothing the door says, the door's identity was proven by the
    // handshake, and every door reads the greeting, greets back and only then
    // reads the ask. A renewal's ballot is one round trip cheaper, which is
    // what lets a lease hold across a region (G075 C3).
    if let Some((tag, body)) = asked(&asking) {
        framed(bound, frame_async::write_tagged(link, tag, &body)).await?;
    }
    let heard = framed(bound, hear(link)).await?;

    match asking {
        Ask::Nothing => Ok((heard, Answered::Nothing)),
        Ask::Ballot(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Vote) => Ok((heard, Answered::Voted(Vote::decode(&body)?))),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Records(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Collected) => {
                    Ok((heard, Answered::Collected(Collected::decode(&body)?)))
                }
                // The refusal the leader sent, rebuilt as the value it was on
                // the other side. A follower that received this as a closed
                // socket would be looking for a network fault instead of
                // reading the one sentence that says what to do.
                Some(PeerFrame::Uncollectable) => {
                    let (from, _) = frame::take_u64(&body, 0)?;
                    Err(Error::Uncollectable { from })
                }
                Some(PeerFrame::Unsubscribed) => Err(Error::Unsubscribed),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Join(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Joined) => match body.as_slice() {
                    [bound] => Ok((heard, Answered::Joined(*bound == 1))),
                    _ => Err(Error::Malformed),
                },
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Attempt(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Attempted) => match body.as_slice() {
                    [permitted] => Ok((heard, Answered::Attempted(*permitted == 1))),
                    _ => Err(Error::Malformed),
                },
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Across(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::AcrossDone) => Ok((
                    heard,
                    Answered::Across(
                        tessari_session::AcrossAnswer::decode(&body)
                            .map_err(|_| Error::Malformed)?,
                    ),
                )),
                // The leader's refusal — its kind, then its words — which the
                // coordinator treats as *not prepared* and answers its caller
                // by the kind (Q-924).
                Some(PeerFrame::NotAcross) => Err(tessari_session::PartRefused::decode(&body)
                    .map_or(Error::Malformed, Error::RefusedAcross)),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Coordinate(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Coordinated) => Ok((
                    heard,
                    Answered::Coordinated(crate::coordination::decode_answer(&body)?),
                )),
                Some(PeerFrame::NotCoordinated) => Err(Error::NotCoordinated(
                    String::from_utf8_lossy(&body).into_owned(),
                )),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Gather(_) => {
            let (tag, body) = framed(bound, answer(link)).await?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Gathered) => Ok((heard, Answered::Gathered(Page::decode(&body)?))),
                // The refusal as the leader sent it, for the reason the two
                // collection refusals above cross the wire as frames.
                Some(PeerFrame::NotGathered) => {
                    let why = body.first().copied().ok_or(Error::Malformed)?;
                    Err(Error::NotGathered(Ungathered::from_byte(why)?))
                }
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
    }
}

/// The frame `asking` puts on the link, or `None` when nothing is asked.
fn asked(asking: &Ask<'_>) -> Option<(u8, Vec<u8>)> {
    match asking {
        Ask::Nothing => None,
        Ask::Ballot(ballot) => Some((PeerFrame::Ballot.tag(), ballot.encode())),
        Ask::Records(collect) => Some((PeerFrame::Collect.tag(), collect.encode())),
        Ask::Join(token) => Some((PeerFrame::Join.tag(), token.to_vec())),
        Ask::Attempt(attempt) => Some((PeerFrame::Attempt.tag(), attempt.encode())),
        Ask::Across(carried) => Some((PeerFrame::Across.tag(), carried.encode())),
        Ask::Coordinate(request) => Some((PeerFrame::Coordinate.tag(), request.encode())),
        Ask::Gather(gather) => Some((PeerFrame::Gather.tag(), gather.encode())),
    }
}

/// The one frame that answers the one follow-up.
pub(super) async fn answer(link: &mut (impl AsyncRead + Unpin)) -> Result<(u8, Vec<u8>)> {
    frame_async::read_tagged(link)
        .await?
        .ok_or(Error::Truncated)
}

/// Put one greeting on the link.
pub(crate) async fn say(link: &mut (impl AsyncWrite + Unpin), hello: &Hello) -> Result<()> {
    frame_async::write_tagged(link, PeerFrame::Hello.tag(), &hello.encode()).await
}

/// Take one greeting off the link, and refuse anything else.
pub(crate) async fn hear(link: &mut (impl AsyncRead + Unpin)) -> Result<Hello> {
    let Some((tag, body)) = frame_async::read_tagged(link).await? else {
        return Err(Error::Truncated);
    };
    greeting(tag, &body)
}

/// A frame that has to be a greeting, and is refused as anything else.
///
/// Shared with the door on the runtime, which reads the frame its own way.
pub(crate) fn greeting(tag: u8, body: &[u8]) -> Result<Hello> {
    match PeerFrame::from_tag(tag) {
        Some(PeerFrame::Hello) => Hello::decode(body),
        Some(_) => Err(Error::OutOfTurn { tag }),
        None => Err(Error::UnknownFrame { tag }),
    }
}
