//! Who is connected right now, for the console's connection list.

use super::*;

/// Every surface this process is serving, and when the process started.
///
/// # Why one surface cannot report on its own
///
/// A metrics route lives on **one** surface but must describe the **process**:
/// "open connections per surface" includes the wire protocol, whose counters an
/// HTTP node has never seen. Something has to hold both, and the process already
/// does — it enumerates every surface to sequence the shutdown. This is that
/// same list, shared rather than rebuilt, so the numbers a scrape reports and
/// the numbers a drain waits on cannot drift apart.
///
/// The process start is here rather than beside it because it is the same kind
/// of fact: true of the process, not of any one listener.
#[derive(Debug)]
pub struct Census {
    started: Instant,
    surfaces: Vec<(&'static str, Arc<Stopping>)>,
    presented: Vec<(&'static str, Presenting)>,
}

/// Reads the certificate a surface presents now (ADR-0108 D6).
///
/// A reader rather than a certificate, because a renewal replaces what a
/// surface presents while the process runs, and a scrape must report the one
/// in use rather than the one the process started with.
pub struct Presenting(
    Box<dyn Fn() -> Option<rustls::pki_types::CertificateDer<'static>> + Send + Sync>,
);

impl Presenting {
    /// A reader over `leaf`.
    pub fn new(
        leaf: impl Fn() -> Option<rustls::pki_types::CertificateDer<'static>> + Send + Sync + 'static,
    ) -> Self {
        Self(Box::new(leaf))
    }
}

impl std::fmt::Debug for Presenting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Presenting")
    }
}

impl Census {
    /// A census of a process that started at `started`.
    ///
    /// Taken from the caller rather than read here, because the interesting
    /// moment is when the **process** began and this is built later — after the
    /// store is open, which is exactly the interval a restart-detector cares
    /// about and would be silently excluded.
    #[must_use]
    pub const fn since(started: Instant) -> Self {
        Self {
            started,
            surfaces: Vec::new(),
            presented: Vec::new(),
        }
    }

    /// Count `name` among this process's surfaces.
    pub fn counting(&mut self, name: &'static str, stopping: Arc<Stopping>) {
        self.surfaces.push((name, stopping));
    }

    /// How long the process has been running.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Each surface, by name.
    pub fn surfaces(&self) -> impl Iterator<Item = (&'static str, &Stopping)> {
        self.surfaces
            .iter()
            .map(|(name, stopping)| (*name, stopping.as_ref()))
    }

    /// Report the certificate `surface` presents.
    pub fn presenting(&mut self, surface: &'static str, leaf: Presenting) {
        self.presented.push((surface, leaf));
    }

    /// The date each presenting surface's certificate expires, as Unix seconds;
    /// `None` for a certificate whose date could not be read.
    pub fn expiries(&self) -> impl Iterator<Item = (&'static str, Option<i64>)> + '_ {
        self.presented
            .iter()
            .map(|(surface, leaf)| (*surface, (leaf.0)().as_ref().and_then(tls::not_after)))
    }
}
