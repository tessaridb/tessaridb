//! One Kafka connection: frames in, answers out, in the order asked.
//!
//! Requests on a connection are answered in order, which the protocol
//! requires, so they are handled one at a time; a client wanting concurrency
//! opens more connections, as every client already does.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tessari_constants::{FRAME_STALL_SECONDS, KAFKA_IDLE_SECONDS, KAFKA_MAX_FRAME_BYTES};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use zeroize::Zeroize as _;

use crate::api::{self, fetch, init_producer, list_offsets, metadata, produce, sasl, versions};
use crate::codec::{Reader, Writer};
use crate::header::{RequestHeader, write_response_header};
use crate::surface::grouping::{self, Grouped};
use crate::surface::running::{Failed, Held};
use crate::surface::{Shared, fetching, metadata as describing, offsets, producing};

/// How often an idle connection looks at whether the node is stopping.
const WATCH_STOPPING: Duration = Duration::from_secs(1);

/// The cluster id Metadata gives: a constant, since this node is the cluster.
const CLUSTER_ID: &str = "tessaridb";

/// Where a connection stands in the SASL exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sasl {
    /// Nothing asked: the connection runs as an anonymous session.
    Open,
    /// A PLAIN handshake was accepted; only an authenticate may follow.
    Handshaken,
    /// Signed in.
    Signed,
}

/// What handling a request leaves to send.
enum Reply {
    /// An answer to frame and send.
    Answer(Vec<u8>),
    /// Nothing: a produce with `acks = 0`.
    Silent,
    /// Close the connection: the request cannot be answered in the protocol.
    Close(&'static str),
}

/// Serve one connection until the client closes it, it idles out, or the node
/// stops.
pub(crate) async fn converse<S>(
    shared: Arc<Shared>,
    reached: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    mut stream: S,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut held = Held::anonymous(&shared);
    let mut sasl = Sasl::Open;
    loop {
        let Some(length) = next_length(&shared, &mut stream).await? else {
            return Ok(());
        };
        let length = usize::try_from(length)
            .ok()
            .filter(|length| *length <= KAFKA_MAX_FRAME_BYTES)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "a frame past the ceiling")
            })?;
        let mut body = vec![0_u8; length];
        tokio::time::timeout(
            Duration::from_secs(FRAME_STALL_SECONDS),
            stream.read_exact(&mut body),
        )
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "a frame stalled"))??;
        let busy = shared.stopping.busy();
        let reply = handle(&shared, &mut held, &mut sasl, (reached, peer), &body).await;
        // A SASL token carries a password; the frame that held it is wiped.
        body.zeroize();
        match reply {
            Reply::Answer(answer) => {
                let size = u32::try_from(answer.len()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "an answer past the ceiling",
                    )
                })?;
                stream.write_all(&size.to_be_bytes()).await?;
                stream.write_all(&answer).await?;
                stream.flush().await?;
            }
            Reply::Silent => {}
            Reply::Close(why) => {
                tracing::info!(reason = why, "kafka connection closed by the node");
                return Ok(());
            }
        }
        shared.stopping.answered(false);
        drop(busy);
    }
}

/// The next frame's length, or `None` when the connection ended or idled out.
async fn next_length<S: AsyncRead + Unpin>(
    shared: &Shared,
    stream: &mut S,
) -> std::io::Result<Option<i32>> {
    let mut size = [0_u8; 4];
    let mut idle = Duration::ZERO;
    loop {
        match tokio::time::timeout(WATCH_STOPPING, stream.read_exact(&mut size)).await {
            Ok(Ok(_)) => return Ok(Some(i32::from_be_bytes(size))),
            Ok(Err(why)) if why.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Ok(Err(why)) => return Err(why),
            Err(_) => {
                idle = idle.saturating_add(WATCH_STOPPING);
                if shared.stopping.asked() || idle >= Duration::from_secs(KAFKA_IDLE_SECONDS) {
                    return Ok(None);
                }
            }
        }
    }
}

async fn handle(
    shared: &Arc<Shared>,
    held: &mut Held,
    sasl: &mut Sasl,
    (reached, peer): (Option<SocketAddr>, Option<SocketAddr>),
    body: &[u8],
) -> Reply {
    let mut reader = Reader::new(body);
    let Ok(header) = RequestHeader::read(&mut reader) else {
        return Reply::Close("an unreadable request header");
    };
    let spoken = api::served(header.key).filter(|served| served.serves(header.version));
    let mut writer = Writer::with_capacity(256);
    write_response_header(&mut writer, &header);
    if spoken.is_none() {
        if header.key == api::API_VERSIONS {
            return match versions::answer(&mut writer, header.version) {
                Ok(()) => Reply::Answer(writer.into_bytes()),
                Err(_) => Reply::Close("an answer that could not be written"),
            };
        }
        return Reply::Close("an api or version this node does not answer");
    }
    if *sasl == Sasl::Handshaken && header.key != SASL_AUTHENTICATE {
        return Reply::Close("a request between the SASL handshake and its authenticate");
    }
    let (version, flexible) = (header.version, header.flexible);
    let answered = match header.key {
        api::API_VERSIONS => versions::answer(&mut writer, version),
        SASL_HANDSHAKE => match sasl::read_handshake(&mut reader) {
            Ok(mechanism) if mechanism == sasl::PLAIN && *sasl == Sasl::Open => {
                *sasl = Sasl::Handshaken;
                sasl::write_handshake(&mut writer, 0)
            }
            Ok(_) if *sasl != Sasl::Open => {
                sasl::write_handshake(&mut writer, sasl::ILLEGAL_SASL_STATE)
            }
            Ok(_) => sasl::write_handshake(&mut writer, sasl::UNSUPPORTED_SASL_MECHANISM),
            Err(_) => return Reply::Close("an unreadable SASL handshake"),
        },
        SASL_AUTHENTICATE => {
            let Ok(token) = sasl::read_authenticate(&mut reader, flexible) else {
                return Reply::Close("an unreadable SASL token");
            };
            let (code, message) = match sasl::Plain::read(token) {
                Ok(plain) => match held.sign_in(shared, plain.user, plain.password).await {
                    Ok(()) => {
                        *sasl = Sasl::Signed;
                        (0, None)
                    }
                    Err(Failed::Refused(refused)) => {
                        (sasl::SASL_AUTHENTICATION_FAILED, Some(refused.to_string()))
                    }
                    Err(Failed::Busy) => (
                        sasl::SASL_AUTHENTICATION_FAILED,
                        Some("the node is busy; try again".to_owned()),
                    ),
                    Err(Failed::Lost) => return Reply::Close("the sign-in panicked"),
                    // A sign-in is never carried.
                    Err(Failed::Carried(refused)) => {
                        (sasl::SASL_AUTHENTICATION_FAILED, Some(refused.message))
                    }
                },
                Err(why) => (sasl::SASL_AUTHENTICATION_FAILED, Some(why.to_string())),
            };
            let written =
                sasl::write_authenticate(&mut writer, version, flexible, code, message.as_deref());
            if code != 0 {
                // A failed sign-in is answered and the connection then closed,
                // as a broker does: nothing else may be sent on it.
                return match written {
                    Ok(()) => Reply::Answer(writer.into_bytes()),
                    Err(_) => Reply::Close("an answer that could not be written"),
                };
            }
            written
        }
        METADATA => match metadata::Request::read(&mut reader, version, flexible) {
            Ok(request) => {
                let brokers = describing::brokers(shared, held, reached).await;
                let topics =
                    describing::topics(shared, held, &brokers, request.topics.as_deref()).await;
                // The controller is one broker for the whole cluster, whichever
                // node is asked: the one coordinating its groups.
                let controller = brokers.coordinator(shared).await.node_id;
                metadata::write_response(
                    &mut writer,
                    version,
                    flexible,
                    &brokers.listed,
                    controller,
                    CLUSTER_ID,
                    &topics,
                )
            }
            Err(_) => return Reply::Close("an unreadable metadata request"),
        },
        PRODUCE => match produce::Request::read(&mut reader, flexible) {
            Ok(request) => {
                let answers = producing::produce(shared, held, &request).await;
                if request.acks == 0 {
                    return Reply::Silent;
                }
                produce::write_response(&mut writer, version, flexible, &answers)
            }
            Err(_) => return Reply::Close("an unreadable produce request"),
        },
        FETCH => match fetch::Request::read(&mut reader, version, flexible) {
            Ok(request) => {
                let answers = fetching::fetch(shared, held, &request).await;
                fetch::write_response(&mut writer, version, flexible, &answers)
            }
            Err(_) => return Reply::Close("an unreadable fetch request"),
        },
        LIST_OFFSETS => match list_offsets::read_request(&mut reader, version, flexible) {
            Ok(topics) => {
                let answers = offsets::list(shared, held, &topics).await;
                list_offsets::write_response(&mut writer, version, flexible, &answers)
            }
            Err(_) => return Reply::Close("an unreadable list offsets request"),
        },
        INIT_PRODUCER_ID => match init_producer::read_request(&mut reader, version, flexible) {
            Ok(true) => {
                init_producer::write_response(
                    &mut writer,
                    flexible,
                    init_producer::TRANSACTIONAL_ID_AUTHORIZATION_FAILED,
                    -1,
                    -1,
                );
                Ok(())
            }
            Ok(false) => {
                init_producer::write_response(
                    &mut writer,
                    flexible,
                    0,
                    shared.producers.issue(),
                    0,
                );
                Ok(())
            }
            Err(_) => return Reply::Close("an unreadable init producer id request"),
        },
        _ => {
            let from = grouping::From {
                header: &header,
                reached,
                peer,
            };
            match grouping::answer(shared, held, &from, &mut reader, &mut writer).await {
                Grouped::Written(written) => written,
                Grouped::Unreadable => return Reply::Close("an unreadable request body"),
                Grouped::NotMine => return Reply::Close("an api this node has not taken on yet"),
            }
        }
    };
    match answered {
        Ok(()) => Reply::Answer(writer.into_bytes()),
        Err(_) => Reply::Close("an answer that could not be written"),
    }
}

const PRODUCE: i16 = 0;
const FETCH: i16 = 1;
const LIST_OFFSETS: i16 = 2;
const METADATA: i16 = 3;
const SASL_HANDSHAKE: i16 = 17;
const INIT_PRODUCER_ID: i16 = 22;
const SASL_AUTHENTICATE: i16 = 36;
