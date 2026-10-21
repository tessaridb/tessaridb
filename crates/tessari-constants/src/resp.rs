//! The RESP surface's ceilings and defaults (ADR-0129).

/// The port a node serves RESP on when told to serve it without an address.
///
/// The protocol's own conventional port, so a client's address changes host
/// and nothing else (owner, 2026-10-21).
pub const RESP_DEFAULT_PORT: u16 = 6379;

/// The largest single bulk string read from a RESP client.
///
/// Unit: bytes.
///
/// The wire and HTTP surfaces' ceiling, so no surface accepts a value another
/// refuses; a cache server's own limit is larger, and a client sending more is
/// told so rather than holding the node's memory for it.
pub const RESP_MAX_BULK_BYTES: usize = 16 * 1024 * 1024;

/// The most arguments one RESP command may carry.
///
/// A count is the first thing a frame states, before any of its arguments
/// arrive; refusing a large count up front means it is never allocated for.
pub const RESP_MAX_ARGUMENTS: usize = 1_048_576;

/// The most bytes one command's arguments may hold together.
///
/// Unit: bytes.
///
/// Many arguments each below the bulk ceiling would otherwise add up to many
/// times it.
pub const RESP_MAX_COMMAND_BYTES: usize = 64 * 1024 * 1024;

/// How long a RESP connection may sit between commands before it is closed.
///
/// Unit: seconds.
///
/// The Kafka surface's ten minutes: an idle place held forever is a place
/// another client cannot have.
pub const RESP_IDLE_SECONDS: u64 = 600;

/// The most keys one `KEYS` or `SCAN` page walks.
///
/// A `KEYS *` over a large space would otherwise read it whole in one answer;
/// a client paging with `SCAN` asks again from where the page stopped.
pub const RESP_SCAN_PAGE: u64 = 1_000;

/// The most `SCAN` cursors one RESP connection holds at once.
///
/// A cursor is a number a client hands back, and the place it stands for is
/// kept on the connection; past this many, the oldest is forgotten and a client
/// that hands it back is told to start again.
pub const RESP_SCAN_CURSORS: usize = 1_024;

/// The most commands one `MULTI` may queue before its `EXEC`.
///
/// The whole queue runs as one transaction, and a transaction that grows
/// without bound is memory the node holds for one connection.
pub const RESP_MULTI_COMMANDS: usize = 10_000;

/// The most commands of a pipeline answered before the answers are flushed.
///
/// A client may send a thousand commands before reading one answer; flushing
/// every this many keeps the answers it has not read from piling up in the
/// node, and keeps one connection from holding its worker for a whole backlog.
pub const RESP_PIPELINE_COMMANDS: usize = 1_024;

/// The largest table an `LCS` may build: the product of the two lengths.
///
/// Unit: cells, four bytes each.
///
/// The answer needs the whole table, so two values a megabyte each would ask
/// for four terabytes; past this the command is refused rather than run.
pub const RESP_LCS_MAX_CELLS: usize = 4 * 1024 * 1024;

/// The longest inline command — a line typed into a terminal — read before a
/// newline arrives.
///
/// Unit: bytes.
///
/// An inline command has no length up front, so without a ceiling a client
/// that never sends a newline is a buffer that never stops growing.
pub const RESP_INLINE_MAX_BYTES: usize = 64 * 1024;

/// How long a command that reads a value, changes it and writes it back
/// (`APPEND`, `SETRANGE`, `INCRBYFLOAT`) keeps trying when the key changed
/// between its read and its write.
///
/// Unit: milliseconds.
///
/// The store's own window for a lone conditional write that lost a race: a
/// budget in time rather than in tries, so four writers on one key all land
/// rather than the unlucky one giving up after a fixed count.
pub const RESP_CHANGE_PATIENCE_MILLIS: u64 = 1_000;
