//! Event bodies and conditions already parsed, keyed by their text (Q-912).
//!
//! # Why this exists
//!
//! An event runs inside every write it is declared for, and its body and its
//! condition were parsed again on each of them — measured with the rest of an
//! event's overhead at G055 W7, and the one part that depends on nothing but
//! the text. Here the text is parsed once per process and the parsed form is
//! cloned for each run, which binds its own copy.
//!
//! # Why it cannot serve a stale body
//!
//! The key is the text itself, as the catalog holds it and as the write read
//! it. A redefined event is different text and misses; a transaction reading an
//! older catalog reads the older text and gets the older body. Parsing is a
//! function of the text alone, so there is nothing a key can go stale against
//! and nothing to invalidate.
//!
//! # Bound
//!
//! [`HELD`] texts of each kind, plus at most one per writer missing at the same
//! moment; past it the set is dropped and refilled by what is running now — the
//! rule decoded table definitions keep, and for the same reason.

use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use tessari_ql::{Expr, Script};

use crate::error::Result;

/// How many texts of each kind a process keeps parsed.
const HELD: usize = 256;

static BODIES: LazyLock<DashMap<String, Arc<Script>>> = LazyLock::new(DashMap::new);
static CONDITIONS: LazyLock<DashMap<String, Arc<Expr>>> = LazyLock::new(DashMap::new);

/// An event's body, parsed.
///
/// # Errors
///
/// The parse failure of a body that does not parse.
pub(super) fn body(text: &str) -> Result<Script> {
    Ok(parsed(&BODIES, text, tessari_ql::parse)?.as_ref().clone())
}

/// An event's `WHEN` condition, parsed.
///
/// # Errors
///
/// The parse failure of a condition that does not parse.
pub(super) fn condition(text: &str) -> Result<Expr> {
    Ok(parsed(&CONDITIONS, text, tessari_ql::parse_expression)?
        .as_ref()
        .clone())
}

fn parsed<T>(
    held: &DashMap<String, Arc<T>>,
    text: &str,
    parse: impl FnOnce(&str) -> std::result::Result<T, tessari_ql::Error>,
) -> Result<Arc<T>> {
    // The guard ends with this statement, before anything below writes.
    if let Some(found) = held.get(text).map(|found| Arc::clone(&found)) {
        return Ok(found);
    }
    let fresh = Arc::new(parse(text)?);
    if held.len() >= HELD {
        held.clear();
    }
    held.insert(text.to_owned(), Arc::clone(&fresh));
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn one_text_is_parsed_once_and_another_text_is_another_body() {
        let held: DashMap<String, Arc<Script>> = DashMap::new();
        let first = parsed(&held, "CREATE log = { n: 1 };", tessari_ql::parse).unwrap();
        let again = parsed(&held, "CREATE log = { n: 1 };", tessari_ql::parse).unwrap();
        let other = parsed(&held, "CREATE log = { n: 2 };", tessari_ql::parse).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_ne!(*first, *other);
        assert_eq!(*first, tessari_ql::parse("CREATE log = { n: 1 };").unwrap());
    }

    #[test]
    fn past_its_bound_the_set_is_refilled_rather_than_grown() {
        let held: DashMap<String, Arc<Expr>> = DashMap::new();
        for n in 0..HELD.saturating_mul(2) {
            parsed(&held, &format!("{n} > 1"), tessari_ql::parse_expression).unwrap();
            assert!(held.len() <= HELD);
        }
    }

    #[test]
    fn a_text_that_does_not_parse_is_refused_and_not_kept() {
        let held: DashMap<String, Arc<Script>> = DashMap::new();
        assert!(parsed(&held, "CREATE log = {", tessari_ql::parse).is_err());
        assert!(held.is_empty());
    }
}
