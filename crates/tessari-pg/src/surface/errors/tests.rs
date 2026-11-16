//! The error block's contract: what the surface itself refuses with, and the
//! invariant that the store's words travel verbatim.

use super::{Refused, code};

#[test]
fn a_surface_refusal_carries_its_code_and_words() {
    let refused = Refused::surface(code::SYNTAX, "a name was expected");
    assert_eq!(refused.code, "42601");
    assert_eq!(refused.message, "a name was expected");
    assert_eq!(refused.detail, None);
}

#[test]
fn a_detail_names_what_was_found() {
    let refused = Refused::with_detail(
        code::INVALID_CATALOG,
        "the form is <namespace>.<database>",
        "got \"shop\"",
    );
    assert_eq!(refused.code, "3D000");
    assert_eq!(refused.detail.as_deref(), Some("got \"shop\""));
}

#[test]
fn the_codes_are_the_dialects_own() {
    // A compile-time table of the codes this build answers with: the wire
    // spells five characters, and each is a real class of the dialect.
    for spelled in [
        code::SYNTAX,
        code::FEATURE,
        code::INVALID_PASSWORD,
        code::INVALID_AUTHORIZATION,
        code::INVALID_CATALOG,
        code::FORBIDDEN,
        code::SERIALIZATION,
        code::UNAVAILABLE,
        code::THROTTLED,
        code::INTERNAL,
    ] {
        assert_eq!(spelled.len(), 5, "{spelled} is not five characters");
        assert!(
            spelled.chars().all(|c| c.is_ascii_alphanumeric()),
            "{spelled} carries characters the wire does not"
        );
    }
}
