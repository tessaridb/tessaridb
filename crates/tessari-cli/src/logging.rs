//! What a running node reports, and where it goes.
//!
//! The library crates emit `tracing` events and spans and install nothing, so a
//! caller that embeds this store sees nothing unless it installs a subscriber of
//! its own. The binary is where that choice is made, and this is the choice:
//! one event per line on standard error, filtered by `TESSARIDB_LOG`, as text
//! a person reads or as JSON a collector reads (`TESSARIDB_LOG_FORMAT`).
//!
//! # What a line carries
//!
//! The time in UTC to the microsecond, the level, the module that spoke, the
//! spans the event happened inside — `connection{connection=7 peer=…}` — then a
//! sentence and the values as named fields. A value is never spliced into the
//! sentence: `records=3` can be searched for and summed, and "pruned 3 log
//! record(s)" can only be read.
//!
//! # Colour only where somebody is looking
//!
//! Levels and field names are coloured when standard error is a terminal and
//! `NO_COLOR` is not set, and plain otherwise, so a container's log driver and a
//! file never receive escape codes.
//!
//! # Written off the thread that spoke
//!
//! Events go through a queue to one writer thread, so a slow terminal or a full
//! pipe never stalls a connection or a store call. The queue is flushed when the
//! guard [`install`] returns is dropped at the end of `main`, and by [`abort`],
//! which the paths that end the node at once go through, so the line explaining
//! why is the last one written rather than one left in the queue.

use std::io::IsTerminal;
use std::sync::{Mutex, PoisonError};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::SystemTime;

/// The variable that sets how much is reported: a level, or directives such as
/// `info,tessari_wire=debug`.
const LEVEL: &str = "TESSARIDB_LOG";

/// The variable that chooses the line format: `text` (the default) or `json`.
const FORMAT: &str = "TESSARIDB_LOG_FORMAT";

/// Report at this level when nothing says otherwise.
///
/// `info` rather than `warn`: the events this exists for — a connection
/// accepted, a sign-in refused — are not warnings, and a node that reports only
/// its problems cannot answer "was it even reached".
const DEFAULT: &str = "info";

/// The writer's guard, where [`abort`] can reach it.
///
/// Held here rather than only in `main` because the paths that end the process
/// at once never return to `main`, and dropping the guard is what flushes.
static WRITER: Mutex<Option<WorkerGuard>> = Mutex::new(None);

/// Flushes what was reported when it goes out of scope; hold it for all of `main`.
#[must_use = "dropping this flushes and stops the log writer"]
pub struct Flushing;

impl Drop for Flushing {
    fn drop(&mut self) {
        drop(WRITER.lock().unwrap_or_else(PoisonError::into_inner).take());
    }
}

/// How the lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Json,
}

/// Install the subscriber, reading the filter and the format from the
/// environment.
///
/// A value it cannot read is not an error: refusing to start a database because
/// a log setting was misspelled would be a worse outcome than reporting at the
/// level and in the format it would have used anyway. It says so instead, once,
/// as the first thing it reports.
pub fn install() -> Flushing {
    let asked_level = std::env::var(LEVEL).ok();
    let asked_format = std::env::var(FORMAT).ok();
    let (filter, unread_level) = filter(asked_level.as_deref());
    let (format, unread_format) = format(asked_format.as_deref());
    let (writer, guard) = tracing_appender::non_blocking(std::io::stderr());
    let coloured = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let installed = match format {
        Format::Text => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_timer(SystemTime)
            .with_ansi(coloured)
            .with_writer(writer)
            .try_init(),
        Format::Json => tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_env_filter(filter)
            .with_timer(SystemTime)
            .with_writer(writer)
            .try_init(),
    };
    // A subscriber an embedding caller installed first has won, which is the
    // right outcome; this one's writer is then simply not kept.
    if installed.is_ok() {
        *WRITER.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard);
    }
    if let Some(unread) = unread_level {
        tracing::warn!(variable = LEVEL, value = %unread, using = DEFAULT, "unreadable log filter");
    }
    if let Some(unread) = unread_format {
        tracing::warn!(variable = FORMAT, value = %unread, using = "text", "unreadable log format");
    }
    Flushing
}

/// End the process at once, after writing out what was reported.
///
/// Aborting rather than exiting keeps the reason these paths exist — nothing
/// runs teardown on a process already known to be wrong — and flushing first
/// keeps the one line that says why.
pub fn abort() -> ! {
    drop(WRITER.lock().unwrap_or_else(PoisonError::into_inner).take());
    std::process::abort()
}

/// The filter `asked` describes, or the default and the text it could not read.
fn filter(asked: Option<&str>) -> (EnvFilter, Option<String>) {
    let Some(asked) = asked.map(str::trim).filter(|asked| !asked.is_empty()) else {
        return (EnvFilter::new(DEFAULT), None);
    };
    match EnvFilter::try_new(asked.to_ascii_lowercase()) {
        Ok(filter) => (filter, None),
        Err(_) => (EnvFilter::new(DEFAULT), Some(asked.to_owned())),
    }
}

/// The format `asked` names, or text and the word it could not read.
fn format(asked: Option<&str>) -> (Format, Option<String>) {
    match asked
        .map(|asked| asked.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("" | "text") => (Format::Text, None),
        Some("json") => (Format::Json, None),
        Some(_) => (Format::Text, asked.map(str::to_owned)),
    }
}

#[cfg(test)]
mod tests;
