use super::*;

/// A password as written, which prints as `<redacted>` and nothing else.
///
/// [`render`](crate::render) refuses to turn `DEFINE USER` back into text, so
/// that a credential cannot be recovered from a statement the store is holding.
/// A `String` field inside a derived `Debug` gives it back in one
/// interpolation, and the line that does it is always somewhere else and
/// written later — the same reasoning `tessari-wire` writes out over its own
/// hand-written `Debug` for a request.
///
/// The plaintext is reachable only through [`Password::expose`], so every place
/// that reads it is a place somebody chose to write that name.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// Hold a password the parser has just read.
    #[must_use]
    pub const fn new(text: String) -> Self {
        Self(text)
    }

    /// The plaintext, for the one caller that hashes it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// What `DEFINE USER` is given to sign the user in with (ADR-0091).
///
/// A password is hashed before it is stored; a hash is stored as given, which is
/// how a state script re-creates a user it could never have known the password
/// of. Both print as `<redacted>`: a hash is not the password, but it is what an
/// offline guess is checked against, and nothing needs it in a log line.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// `PASSWORD '…'` — the plaintext, hashed on the way in.
    Password(Password),
    /// `PASSHASH '$argon2id$…'` — a hash this store would itself have produced.
    Hash(String),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Password(_) => "Password(<redacted>)",
            Self::Hash(_) => "Hash(<redacted>)",
        })
    }
}

/// The one field an [`AlterUser`](StatementKind::AlterUser) statement changes.
///
/// One per statement rather than a record of optional fields, because the
/// difference matters at the point of writing: a struct of `Option`s makes
/// "leave the password alone" and "set the password to nothing" the same shape,
/// and the executor then has to be trusted to tell them apart. Here the
/// statement carries only what it came to change, and nothing else can be
/// touched by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserChange {
    /// `SET PASSWORD '…'` — a new credential, hashed before it is stored.
    Password(Password),
    /// `SET ROLE editor` — what the user may do, within the tenancy they
    /// already hold. The tenancy itself does not move.
    Role(Name),
}

/// What a `DEFINE USER` says the user may do.
///
/// Two spellings of one thing: a role is a *name for a set*, and the set is
/// what the store keeps. Both are kept because dropping the role would make
/// every existing statement and every existing record wrong to gain nothing —
/// three names cover the common cases, and the set covers the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserGrant {
    /// `ROLE editor` — the bundle that name stands for.
    Role(Name),
    /// `AUTHORITIES manage, read` — the set, said directly.
    ///
    /// This is what makes the rule a role could not express sayable: a holder
    /// of `manage` at a namespace who holds neither `read` nor `write` there.
    Authorities(Vec<Name>),
}
