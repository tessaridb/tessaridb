//! Authority kinds by name, and the reaches each may be held at.

use crate::error::{Error, Result};
use tessari_ql::{Name, Span};
use tessari_storage::{Kind, Reach};

/// The kind a word names, refused when it names none.
///
/// The refusal carries the whole set rather than only the rejection, because
/// there are six of them and a reader who mistyped one is a reader who does not
/// yet know which six.
pub(crate) fn kind_named(named: &Name) -> Result<Kind> {
    Kind::parse(&named.text).ok_or_else(|| Error::NoSuchAuthority {
        name: named.text.clone(),
        known: Kind::ALL
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>()
            .join(", "),
        span: named.span,
    })
}

/// Refuse a kind named somewhere its kind cannot be held.
///
/// One function called from both statements, because the two are the same
/// question about the same model and a rule written twice is a rule that will
/// one day disagree with itself. `DEFINE USER … AUTHORITIES replicate` and
/// `GRANT replicate ON NAMESPACE …` are the only two places a caller names a
/// kind and a reach together.
///
/// Deliberately not asked of `REVOKE`: taking away an authority nobody can hold
/// removes nothing and is already idempotent, and refusing it would stop an
/// administrator cleaning up a row an older binary wrote.
///
/// # Errors
///
/// [`Error::NotAtThatReach`] when `kind` cannot be held at `reach`.
pub(crate) fn holdable_at(kind: Kind, reach: Reach, span: Span) -> Result<()> {
    if kind.may_be_held_at(reach) {
        return Ok(());
    }
    Err(Error::NotAtThatReach {
        kind: kind.name(),
        span,
    })
}
