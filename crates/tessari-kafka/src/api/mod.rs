//! The APIs this node answers, and the versions of each.
//!
//! The table is the surface's whole claim about itself: ApiVersions answers it,
//! the dispatcher refuses anything outside it, and the flexible threshold is
//! where each API's encoding changes. Ranges and thresholds are the ones the
//! G079 matrix decided against the request schemas of the 3.9 clients.

pub mod fetch;
pub mod init_producer;
pub mod list_offsets;
pub mod metadata;
pub mod produce;
pub mod sasl;
pub mod versions;

/// One API this node answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Api {
    /// Its key.
    pub key: i16,
    /// Its name, for a refusal or a log line.
    pub name: &'static str,
    /// The oldest version answered.
    pub min: i16,
    /// The newest version answered.
    pub max: i16,
    /// The first flexible version, `None` when the API has none.
    pub flexible_from: Option<i16>,
}

impl Api {
    /// Whether `version` uses the flexible encoding.
    #[must_use]
    pub fn is_flexible(&self, version: i16) -> bool {
        self.flexible_from.is_some_and(|from| version >= from)
    }

    /// Whether `version` is answered.
    #[must_use]
    pub const fn serves(&self, version: i16) -> bool {
        version >= self.min && version <= self.max
    }
}

const fn api(key: i16, name: &'static str, min: i16, max: i16, flexible_from: Option<i16>) -> Api {
    Api {
        key,
        name,
        min,
        max,
        flexible_from,
    }
}

/// Every API this node answers, in key order.
///
/// Only what is served: a client reads this table to decide what it may ask,
/// and an API listed before it answers would be met with a closed connection
/// instead of the clear refusal a missing entry gives. The rest of the
/// matrix's rows join it as they are built (G079 W4, W5).
pub const SERVED: &[Api] = &[
    api(0, "Produce", 3, 9, Some(9)),
    api(1, "Fetch", 4, 12, Some(12)),
    api(2, "ListOffsets", 1, 7, Some(6)),
    api(3, "Metadata", 1, 9, Some(9)),
    api(17, "SaslHandshake", 1, 1, None),
    api(18, "ApiVersions", 0, 3, Some(3)),
    api(22, "InitProducerId", 0, 4, Some(2)),
    api(36, "SaslAuthenticate", 0, 2, Some(2)),
];

/// The API with `key`, if this node answers it.
#[must_use]
pub fn served(key: i16) -> Option<&'static Api> {
    SERVED.iter().find(|api| api.key == key)
}

/// The key of ApiVersions, which has rules of its own about headers.
pub const API_VERSIONS: i16 = 18;

#[cfg(test)]
mod tests;
