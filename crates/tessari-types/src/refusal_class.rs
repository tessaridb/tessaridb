//! What a caller should do about a refusal (ADR-0117).
//!
//! The store names some three hundred refusals and keeps adding them; a test
//! asserts a name, but a client cannot branch on a set that moves with every
//! release. It branches on this: nine classes, each named for the caller's next
//! move, closed for 1.x. The wire carries the class as one byte and HTTP as a
//! word, both from this one table.

/// The class of a refusal — what the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusalClass {
    /// Fix the request; repeating it unchanged cannot succeed.
    Invalid,
    /// Sign in, or sign in again.
    Unauthenticated,
    /// Stop: signing in again will not help.
    Forbidden,
    /// Wait, then repeat.
    Throttled,
    /// Send it to the node the refusal names.
    Elsewhere,
    /// Run the transaction again from its start.
    Retry,
    /// Re-read: the state the request assumed is not the state there is.
    Conflict,
    /// Try later or another node; the request itself was fine.
    Unavailable,
    /// A defect, damaged data, or a format this build cannot read — report it.
    Internal,
}

impl RefusalClass {
    /// Every class, in byte order.
    pub const ALL: [Self; 9] = [
        Self::Invalid,
        Self::Unauthenticated,
        Self::Forbidden,
        Self::Throttled,
        Self::Elsewhere,
        Self::Retry,
        Self::Conflict,
        Self::Unavailable,
        Self::Internal,
    ];

    /// The byte the wire carries.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Invalid => 1,
            Self::Unauthenticated => 2,
            Self::Forbidden => 3,
            Self::Throttled => 4,
            Self::Elsewhere => 5,
            Self::Retry => 6,
            Self::Conflict => 7,
            Self::Unavailable => 8,
            Self::Internal => 9,
        }
    }

    /// The class a wire byte names.
    ///
    /// `None` for a byte this build does not know. A reader treats that as
    /// [`Self::Internal`] — never as something to retry — because a class it
    /// cannot read is a class whose advice it cannot follow.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Invalid),
            2 => Some(Self::Unauthenticated),
            3 => Some(Self::Forbidden),
            4 => Some(Self::Throttled),
            5 => Some(Self::Elsewhere),
            6 => Some(Self::Retry),
            7 => Some(Self::Conflict),
            8 => Some(Self::Unavailable),
            9 => Some(Self::Internal),
            _ => None,
        }
    }

    /// The word HTTP carries in an error body's `code`.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::Throttled => "throttled",
            Self::Elsewhere => "elsewhere",
            Self::Retry => "retry",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RefusalClass;

    #[test]
    fn every_class_reads_back_from_its_byte_and_has_a_word_of_its_own() {
        let mut words = std::collections::BTreeSet::new();
        for class in RefusalClass::ALL {
            assert_eq!(RefusalClass::from_byte(class.byte()), Some(class));
            assert!(words.insert(class.word()), "{} twice", class.word());
        }
        // Bytes 1–9 in order, so `ALL` and the table cannot drift apart.
        let bytes: Vec<u8> = RefusalClass::ALL.iter().map(|class| class.byte()).collect();
        assert_eq!(bytes, (1..=9).collect::<Vec<u8>>());
    }

    #[test]
    fn a_byte_outside_the_table_names_no_class() {
        for byte in [0_u8, 10, 255] {
            assert_eq!(RefusalClass::from_byte(byte), None);
        }
    }
}
