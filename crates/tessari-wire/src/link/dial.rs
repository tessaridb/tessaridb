use super::*;

/// Reach the peer `at` on `address`, and exchange greetings.
///
/// # Errors
///
/// Returns [`Error::Transport`] when the node reached does not hold a peer
/// credential for `at` — the handshake refuses it, which is why this side needs
/// no admission rule of its own; [`Error::Uncollectable`] when a collection was
/// asked for from a position the peer cannot state a predecessor for; and
/// [`Error::OutOfTurn`] when the answer is not of the kind that was asked for.
pub fn call(
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
}

/// [`call`], with the connect and every read and write bounded by `bound`
/// rather than by the greeting's deadline.
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
pub fn call_within(
    address: impl ToSocketAddrs,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    asking: Ask<'_>,
    bound: Duration,
) -> Result<(Hello, Answered)> {
    let (mut session, mut socket) = open_within(address, (keys, mine), at, bound)?;
    let exchanged = exchange(&mut session, &mut socket, said, asking);

    // Say goodbye properly even when the exchange failed. A TLS peer that just
    // drops the socket makes the other end's next read an error rather than an
    // end, so a caller that skipped this would leave every door it spoke to
    // reporting a fault it did not have.
    session.send_close_notify();
    drop(session.write_tls(&mut socket));
    exchanged
}

/// Reach the peer `at` on `address` and open the TLS session a conversation
/// rides, with every read and write bounded by the greeting's deadline.
pub(crate) fn open(
    address: impl ToSocketAddrs,
    keys: &PeerKeys,
    at: [u8; NODE_ID_LEN],
) -> Result<(ClientConnection, TcpStream)> {
    open_within(
        address,
        (keys, keys.duplicate()),
        at,
        Duration::from_secs(GREETING_SECONDS),
    )
}

/// [`open`], with the connect and every read and write bounded by `bound`.
pub(crate) fn open_within(
    address: impl ToSocketAddrs,
    (keys, mine): (&PeerKeys, Credential),
    at: [u8; NODE_ID_LEN],
    bound: Duration,
) -> Result<(ClientConnection, TcpStream)> {
    let settings = keys.dialling(mine)?;
    let expected = credential::names(at, Purpose::Peer);
    let name = ServerName::try_from(expected).map_err(|why| Error::Transport(why.to_string()))?;
    let session = ClientConnection::new(Arc::new(settings), name)
        .map_err(|why| Error::Transport(why.to_string()))?;

    let socket = connect(address, bound)?;
    // A zero would mean *no timeout at all* to the socket, the opposite of what
    // a spent deadline asks for, so the least a step may have is a millisecond.
    let bound = Some(bound.max(Duration::from_millis(1)));
    socket.set_read_timeout(bound)?;
    socket.set_write_timeout(bound)?;
    Ok((session, socket))
}

/// Open the connection a call rides, giving up after `bound` per address.
///
/// The reads and writes were always bounded and the connect was not, so a peer
/// whose host drops SYNs held the calling round for the kernel's own connect
/// limit — and the rounds dial their peers one after another (Q-836). Each
/// resolved address is tried in turn, as `TcpStream::connect` does.
pub(super) fn connect(address: impl ToSocketAddrs, bound: Duration) -> Result<TcpStream> {
    let mut failed = None;
    for at in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&at, bound) {
            Ok(socket) => {
                socket.set_nodelay(true)?;
                return Ok(socket);
            }
            Err(why) => failed = Some(why),
        }
    }
    Err(failed
        .unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the peer's address resolved to nothing",
            )
        })
        .into())
}

/// The greeting and the one follow-up, on a session that is already open.
pub(super) fn exchange(
    session: &mut ClientConnection,
    socket: &mut TcpStream,
    said: &Hello,
    asking: Ask<'_>,
) -> Result<(Hello, Answered)> {
    let mut link = rustls::Stream::new(session, socket);
    say(&mut link, said)?;
    let heard = hear(&mut link)?;

    match asking {
        Ask::Nothing => Ok((heard, Answered::Nothing)),
        Ask::Ballot(ballot) => {
            frame::write_tagged(&mut link, PeerFrame::Ballot.tag(), &ballot.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Vote) => Ok((heard, Answered::Voted(Vote::decode(&body)?))),
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Records(collect) => {
            frame::write_tagged(&mut link, PeerFrame::Collect.tag(), &collect.encode())?;
            let (tag, body) = answer(&mut link)?;
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
        Ask::Join(token) => {
            frame::write_tagged(&mut link, PeerFrame::Join.tag(), token.as_slice())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Joined) => match body.as_slice() {
                    [bound] => Ok((heard, Answered::Joined(*bound == 1))),
                    _ => Err(Error::Malformed),
                },
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Attempt(asked) => {
            frame::write_tagged(&mut link, PeerFrame::Attempt.tag(), &asked.encode())?;
            let (tag, body) = answer(&mut link)?;
            match PeerFrame::from_tag(tag) {
                Some(PeerFrame::Attempted) => match body.as_slice() {
                    [permitted] => Ok((heard, Answered::Attempted(*permitted == 1))),
                    _ => Err(Error::Malformed),
                },
                Some(_) => Err(Error::OutOfTurn { tag }),
                None => Err(Error::UnknownFrame { tag }),
            }
        }
        Ask::Across(carried) => {
            frame::write_tagged(&mut link, PeerFrame::Across.tag(), &carried.encode())?;
            let (tag, body) = answer(&mut link)?;
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
        Ask::Coordinate(request) => {
            frame::write_tagged(&mut link, PeerFrame::Coordinate.tag(), &request.encode())?;
            let (tag, body) = answer(&mut link)?;
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
        Ask::Gather(gather) => {
            frame::write_tagged(&mut link, PeerFrame::Gather.tag(), &gather.encode())?;
            let (tag, body) = answer(&mut link)?;
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

/// The one frame that answers the one follow-up.
pub(super) fn answer(
    link: &mut rustls::Stream<'_, ClientConnection, TcpStream>,
) -> Result<(u8, Vec<u8>)> {
    frame::read_tagged(link)?.ok_or(Error::Truncated)
}

/// Put one greeting on the link.
pub(crate) fn say(link: &mut impl std::io::Write, hello: &Hello) -> Result<()> {
    frame::write_tagged(link, PeerFrame::Hello.tag(), &hello.encode())
}

/// Take one greeting off the link, and refuse anything else.
pub(crate) fn hear(link: &mut impl std::io::Read) -> Result<Hello> {
    let Some((tag, body)) = frame::read_tagged(link)? else {
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
