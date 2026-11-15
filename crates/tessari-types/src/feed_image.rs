//! Which image of a record a feed's message carries.
//!
//! Kept beside [`WriteKind`] because a feed declaration stores it the way an
//! event declaration stores its write kinds, and the catalog layer below the
//! parser needs the word without depending on the grammar crate.

/// Which image of a record a feed's message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedImage {
    /// The record's key and operation only.
    Keys,
    /// The key, the operation and the images the operation names.
    Full,
}

impl FeedImage {
    /// The word a statement writes it as.
    #[must_use]
    pub const fn spelling(self) -> &'static str {
        match self {
            Self::Keys => "KEYS",
            Self::Full => "FULL",
        }
    }

    /// The image a word names, in any case.
    #[must_use]
    pub fn from_spelling(word: &str) -> Option<Self> {
        [Self::Keys, Self::Full]
            .into_iter()
            .find(|image| image.spelling().eq_ignore_ascii_case(word))
    }
}
