//! An instant and a span, stored rather than computed.
//!
//! Both are a whole-second count plus a sub-second remainder, and both are held
//! in **normalised** form: the remainder is always in `[0, 1_000_000_000)`, so a
//! negative span carries a negative second count and a positive remainder. That
//! is the one representation in which comparing the pair field by field is the
//! same as comparing the quantity, which is what lets these types sit in an
//! index without a comparison function of their own.
//!
//! # No calendar library, deliberately
//!
//! Formatting, parsing, time zones and date arithmetic are not here. They belong
//! to the query language, and a library layered on top of this representation
//! changes nothing about the bytes underneath it. Pulling one in now would be a
//! dependency bought for functions nothing calls yet, and it would put a
//! third-party type in the middle of the store's own value system.

use core::fmt;

/// Sub-second units in one second.
const NANOS_PER_SECOND: u32 = 1_000_000_000;

/// A point in time, as an offset from the Unix epoch in UTC.
///
/// There is no time zone. A zone is a rendering choice made where the value is
/// displayed, and storing one would make two instants that name the same moment
/// compare unequal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Datetime {
    seconds: i64,
    nanos: u32,
}

/// A span of time, which may be negative.
///
/// Distinct from a number because a duration is not a count of anything until
/// someone says of what — treating it as a number is how a value meaning "five
/// minutes" ends up added to one meaning "five bytes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Duration {
    seconds: i64,
    nanos: u32,
}

/// Build the normalised pair, or refuse a remainder that is not one.
///
/// A remainder of a second or more is not a rounding question — it is a caller
/// that has miscounted, and accepting it would produce two different pairs
/// naming one instant.
macro_rules! define_time {
    ($name:ident, $what:literal) => {
        impl $name {
            #[doc = concat!("Build a ", $what, " from whole seconds and a sub-second remainder.")]
            ///
            /// Returns `None` when the remainder is a whole second or more,
            /// because that pair has a second spelling and two spellings of one
            /// value do not compare equal.
            #[must_use]
            pub const fn new(seconds: i64, nanos: u32) -> Option<Self> {
                if nanos >= NANOS_PER_SECOND {
                    return None;
                }
                Some(Self { seconds, nanos })
            }

            #[doc = concat!("A ", $what, " of whole seconds.")]
            #[must_use]
            pub const fn from_seconds(seconds: i64) -> Self {
                Self { seconds, nanos: 0 }
            }

            /// The whole-second count.
            #[must_use]
            pub const fn seconds(self) -> i64 {
                self.seconds
            }

            /// The sub-second remainder, always below one second.
            #[must_use]
            pub const fn nanos(self) -> u32 {
                self.nanos
            }
        }
    };
}

define_time!(Datetime, "instant");
define_time!(Duration, "span");

impl fmt::Display for Datetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}", self.seconds, self.nanos)
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}s", self.seconds, self.nanos)
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_remainder_of_a_whole_second_is_refused_because_it_has_a_second_spelling() {
        assert!(Datetime::new(0, NANOS_PER_SECOND).is_none());
        assert!(Datetime::new(0, NANOS_PER_SECOND.saturating_sub(1)).is_some());
        assert!(Duration::new(0, NANOS_PER_SECOND).is_none());
    }

    #[test]
    fn instants_order_by_the_moment_they_name() {
        let earlier = Datetime::new(10, 5).unwrap();
        let later = Datetime::new(10, 6).unwrap();
        let much_later = Datetime::new(11, 0).unwrap();
        assert!(earlier < later);
        assert!(later < much_later);
    }

    #[test]
    fn a_negative_span_carries_a_negative_second_count_and_a_positive_remainder() {
        // Half a second before the origin: -1 second plus 500 million nanos.
        let before = Duration::new(-1, 500_000_000).unwrap();
        let origin = Duration::from_seconds(0);
        let after = Duration::new(0, 500_000_000).unwrap();

        assert!(before < origin, "a negative span must sort below zero");
        assert!(origin < after);
        // And the field-by-field order is the quantity order, which is the whole
        // reason for the normalised form.
        assert!(before.seconds() < origin.seconds());
    }

    #[test]
    fn whole_second_construction_leaves_no_remainder() {
        assert_eq!(Duration::from_seconds(-3).nanos(), 0);
        assert_eq!(Duration::from_seconds(-3).seconds(), -3);
    }

    #[test]
    fn the_default_is_the_origin() {
        assert_eq!(Datetime::default(), Datetime::from_seconds(0));
        assert_eq!(Duration::default(), Duration::from_seconds(0));
    }
}
