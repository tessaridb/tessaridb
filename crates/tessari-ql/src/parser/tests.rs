use super::{ABSENT, MAX_EXPRESSION_DEPTH, absent_feature, parse_expression};
use crate::error::Error;
use crate::token::Keyword;

/// The deepest statement the reader accepts fits the smallest stack a
/// parsing thread runs on, and one level deeper is refused by name.
///
/// Run on a thread given that stack explicitly, in whatever profile the
/// suite runs in — a debug frame is the larger one — so the ceiling is held
/// to the stack rather than trusted to it. Arrays, `NOT` and `-` are the
/// three ways to nest, and each is walked to the ceiling.
#[test]
fn a_statement_at_the_nesting_ceiling_fits_a_small_stack_and_one_deeper_is_refused() {
    let reader = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let under = MAX_EXPRESSION_DEPTH.saturating_sub(1);
            let arrays = |levels: usize| format!("{}1{}", "[".repeat(levels), "]".repeat(levels));
            let nots = |levels: usize| format!("{}true", "NOT ".repeat(levels));
            let negatives = |levels: usize| format!("{}x", "- ".repeat(levels));
            for shape in [arrays, nots, negatives] {
                assert!(parse_expression(&shape(under)).is_ok(), "{}", shape(under));
                assert!(matches!(
                    parse_expression(&shape(MAX_EXPRESSION_DEPTH)),
                    Err(Error::NestedTooDeep { .. })
                ));
            }
            // Far past the ceiling is the attack, and it is refused as cheaply.
            assert!(matches!(
                parse_expression(&arrays(100_000)),
                Err(Error::NestedTooDeep { .. })
            ));
        })
        .expect("a thread can be started");
    assert!(
        reader.join().is_ok(),
        "the reader overflowed its stack or panicked"
    );
}

/// An entry whose word the lexer reserves can never fire.
///
/// `search` sat here for several milestones after `SEARCH` became a keyword:
/// the lexer stopped producing an identifier for it, so the lookup could not
/// be reached, and the entry went on saying a built feature was missing. The
/// two halves of that failure are separable — this one catches the
/// unreachability, which is the half a reader cannot see.
#[test]
fn no_entry_names_a_word_the_lexer_reserves() {
    for (word, feature) in ABSENT {
        let reserved = Keyword::ALL
            .iter()
            .any(|keyword| keyword.spelling().eq_ignore_ascii_case(word));
        assert!(
            !reserved,
            "`{word}` ({feature}) is a keyword, so this entry can never fire"
        );
    }
}

/// The other half: a word naming something the store has.
///
/// Pins the two the §8 audit found rather than the class, because the class
/// has no test — nothing here can ask whether a feature exists. What keeps
/// the rest honest is the audit, and what this asserts is that these two do
/// not come back.
#[test]
fn a_feature_the_store_has_is_not_reported_absent() {
    assert_eq!(
        absent_feature("search"),
        None,
        "a search index is built — `DEFINE INDEX … SEARCH`"
    );
    assert_eq!(
        absent_feature("knn"),
        None,
        "vector search is built — `DEFINE INDEX … VECTOR euclidean` and `APPROXIMATE`"
    );
    // The two the §8 re-audit found (wave 38), pinned the same way and for
    // the same reason: both told an author that a **built** feature was
    // missing. `SELECT * FROM t OFFSET 5` answered "limiting a result is
    // not in this milestone" while `START 5 LIMIT 2` parses; `PERMISSIONS`
    // answered "per-field permissions" while
    // `GRANT read ON staff FIELDS name TO ada` parses.
    assert_ne!(
        absent_feature("offset"),
        Some("limiting a result"),
        "limiting a result is built — `START` and `LIMIT`"
    );
    assert_ne!(
        absent_feature("permissions"),
        Some("per-field permissions"),
        "per-field permissions are built — `GRANT … ON … FIELDS … TO …`"
    );
}

/// The list still does its job for what really is absent.
///
/// The `OFFSET` line used to read `Some("limiting a result")`, and that is
/// worth leaving a note about: **the test was holding the wrong message in
/// place.** A word naming a built feature survived here precisely because
/// something asserted it, and an assertion is as good at preserving a false
/// statement as at preventing one. What each line pins now is the *spelling*
/// being absent, which is what §8 actually says.
#[test]
fn a_word_naming_an_absent_feature_still_names_it() {
    assert_eq!(absent_feature("having"), Some("filtering groups"));
    assert_eq!(
        absent_feature("OFFSET"),
        Some("a second spelling for `START`")
    );
    assert_eq!(absent_feature("nothing_like_this"), None);
}
