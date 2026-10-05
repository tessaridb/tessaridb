/// The largest single WebSocket frame this node will read from a client.
///
/// Unit: bytes.
///
/// A frame header declares its own payload length before a byte of that payload
/// arrives, and a reader that believes the declaration allocates whatever a
/// stranger asked it to. That is a memory-exhaustion bug with a polite name, so
/// the ceiling is checked against the declared length and the frame is refused
/// before anything is reserved for it.
///
/// Sixty-four kilobytes because of what a client actually sends on this route: a
/// subscription request naming a position and a table, which is tens of bytes. A
/// frame three orders of magnitude larger than the only message the protocol
/// defines is a client fault or an attack, and either way the honest answer is a
/// close rather than an allocation.
pub const SOCKET_MAX_FRAME_BYTES: usize = 64 * 1024;

/// How many store calls one serving surface runs at once.
///
/// Unit: calls in flight.
///
/// Every statement, commit, feed round and catalog read made from the runtime
/// crosses a bridge of this many slots onto the blocking pool, and a caller
/// that finds it full is refused rather than queued (ADR-0085 §2). It is the
/// number the synchronous node held as its connection ceiling, because each of
/// those connections was a thread that could be inside the store at once: the
/// port changed how the store is waited on, not how many may wait.
pub const MAX_STORE_CALLS: usize = 400;

/// How many connections one serving surface holds open at once.
///
/// Unit: connections.
///
/// # Why a ceiling exists at all
///
/// Without one the node has no point at which it refuses: it degrades until
/// something it cannot do without — memory, descriptors — runs out, and the
/// failure arrives at whichever connection happened to be next rather than at
/// the one that caused it. A number here turns that into an answer a client can
/// read.
///
/// # Why this number, and why it is not [`MAX_STORE_CALLS`]
///
/// A held connection used to be an operating-system thread, so the connections
/// a surface held and the store calls it could run were one number. On the
/// runtime a held connection is a task and a few kilobytes of buffers, and a
/// subscriber waiting for a change holds its connection for hours while making
/// no store call at all (ADR-0085 §3). So the two are bounded separately: this
/// many conversations held, of which at most [`MAX_STORE_CALLS`] are inside the
/// store at once.
///
/// Sixteen thousand three hundred and eighty-four holds the ten thousand idle
/// subscribers the port was measured against, with room for the request
/// traffic beside them. It is per **surface**: the wire protocol and the HTTP
/// endpoint each hold their own door, so a flood of one cannot starve the other
/// of the places it needs to answer a health check. A process serving it needs
/// its descriptor limit above it.
pub const MAX_CONNECTIONS: usize = 16_384;

/// How many peer connections the peer door serves at once.
///
/// The door used to serve one: a peer that connected and then said nothing
/// held it for a whole [`GREETING_SECONDS`] per read, and every ballot,
/// greeting and collection from every other peer waited behind it. Each
/// connection is now its own task, so the bound is what keeps a stranger who
/// opens sockets from turning that into memory instead. A peer holds at most a
/// few connections at once — a greeting, a collection, a ballot, a gather — so
/// sixty-four is a cluster of a dozen nodes all calling at the same moment,
/// with room. A connection beyond it is closed unanswered and the peer's next
/// round tries again.
pub const PEER_CONNECTIONS: usize = 64;

/// How long a peer door keeps a link open for the next record of a transaction
/// across leaders, after answering one (ADR-0112 D13j).
///
/// Longer than the coordinator keeps a link idle before it stops offering it
/// ([`ACROSS_KEPT_IDLE_SECONDS`]), so a link the coordinator still uses is never
/// one the door has just closed under it.
pub const ACROSS_DOOR_IDLE_SECONDS: u64 = 10;

/// How long a coordinator offers an idle kept link to its next ask before it
/// opens a fresh one instead (ADR-0112 D13j); see [`ACROSS_DOOR_IDLE_SECONDS`].
pub const ACROSS_KEPT_IDLE_SECONDS: u64 = 4;

/// The most idle links a coordinator keeps to one peer (ADR-0112 D13j): each
/// holds one of that peer's [`PEER_CONNECTIONS`] while it waits, so a dozen
/// peers keeping two each leave most of a door for greetings, ballots and
/// streams.
pub const ACROSS_KEPT_PER_PEER: usize = 2;

/// The largest request body the HTTP surface reads.
///
/// A body is read before its credential is checked, because the credential
/// decides what the body may do — so the read is the one cost an anonymous
/// caller controls, and a read with no ceiling hands them the node's memory.
/// [`MAX_CONNECTIONS`] bounds how many requests run at once, not how large one
/// is.
///
/// Sixteen mebibytes, the same as the wire protocol's frame: no client can
/// send over HTTP a single write the protocol would refuse over the wire. A
/// file larger than this is written in parts, `PUT … START <offset>`.
pub const HTTP_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The largest reassembled WebSocket message this node will read from a client.
///
/// Unit: bytes.
///
/// Separate from the frame ceiling because a message may legally arrive as many
/// fragments, so bounding one frame bounds nothing: a sender can fragment
/// without limit and a reader that only checks each piece accumulates forever.
/// The two ceilings answer two different questions and neither implies the
/// other.
///
/// Equal to the frame ceiling at this size, deliberately — the only message this
/// route defines fits in one frame, so fragmentation here is a proxy's doing
/// rather than a client's need, and a proxy does not enlarge what it forwards.
pub const SOCKET_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The largest WebSocket message `GET /wire` reads from a client (ADR-0089).
///
/// Unit: bytes.
///
/// One wire frame at its 16 MiB body ceiling plus its five-byte header. The
/// socket carries the wire protocol unchanged, so a message smaller than the
/// largest frame the protocol allows would refuse a statement TCP accepts; and
/// the wire's own ceiling still checks every frame inside, so nothing larger
/// is ever needed.
pub const WIRE_SOCKET_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024 + 5;

/// How long a frame that has started may go without another byte arriving.
///
/// Between frames a connection may be idle for as long as it likes: a pooled
/// session and a subscriber both sit quietly for hours, and that is what they
/// are for. Inside a frame it is different — the header announced bytes that
/// have not come, and a peer that stops there holds one of the surface's places
/// for nothing. Measured per read rather than per frame, so a 16 MiB body on a
/// slow link is never cut while it keeps arriving; sixty seconds is the stall
/// nginx allows between two reads of a request body (G061, R-01).
pub const FRAME_STALL_SECONDS: u64 = 60;
