//! The Kafka surface's ceilings and defaults (ADR-0129).

/// The port a node serves the Kafka protocol on when told to serve it without
/// an address, and the one the published image binds.
///
/// The protocol's own conventional port, so a client's bootstrap address
/// changes host and nothing else (owner, 2026-10-21).
pub const KAFKA_DEFAULT_PORT: u16 = 9092;

/// The largest request frame read from a Kafka client.
///
/// Unit: bytes.
///
/// The HTTP body and wire frame ceiling, so no surface accepts a single write
/// another refuses; a producer batching more than this is told
/// `MESSAGE_TOO_LARGE` per partition by a broker as well, and splits.
pub const KAFKA_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// How long a Kafka connection may sit between requests before it is closed.
///
/// Unit: seconds.
///
/// A broker's own default idle close, ten minutes: clients reconnect on their
/// own and expect it, and an idle place held forever is a place another client
/// cannot have.
pub const KAFKA_IDLE_SECONDS: u64 = 600;

/// The longest a fetch waits for a message that has not arrived yet.
///
/// Unit: milliseconds.
///
/// A client asks for its `fetch.max.wait.ms` (half a second by default); a
/// larger ask is held to this, so one connection cannot park a fetch for the
/// length of a client's whole request timeout.
pub const KAFKA_MAX_FETCH_WAIT_MILLIS: u64 = 30_000;

/// The most messages one partition's fetch reads from the store in one answer.
///
/// The fetch's byte budget still trims what is sent; this bounds what is read
/// to find out, so a partition of tiny messages does not walk the whole topic
/// for one answer.
pub const KAFKA_FETCH_MESSAGES: u64 = 1_000;
