//! What makes a conformance corpus unreadable.

/// Why a corpus file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedCorpus {
    /// What is wrong.
    pub reason: String,
    /// Where it is wrong.
    pub line: usize,
}

impl core::fmt::Display for MalformedCorpus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}
