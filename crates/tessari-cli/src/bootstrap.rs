//! The first user, from the environment.
//!
//! # What this is for
//!
//! A store with no users is [open], and declaring the first one closes it. That
//! rule is right for somebody at a terminal and impossible for a container:
//! nothing types the first `DEFINE USER` into a node that came up unattended, so
//! without this the choice is an open store on a network or a start-up script
//! that has to hold a password anyway.
//!
//! So a node reads `TESSARIDB_INITIAL_USER` and `TESSARIDB_INITIAL_PASSWORD` on
//! the way up, and declares that user as a store-wide owner **when the store has
//! no users at all**. It is the same shape every database in a container has
//! settled on, for the same reason.
//!
//! [open]: https://docs.tessaridb.com/security/users
//!
//! # It runs once, and says so the second time
//!
//! A container restarts with the same environment every time, so "already
//! bootstrapped" cannot be a failure — it is the normal case for every start
//! after the first. A store that already has users is left exactly as it is and
//! the node says so at `info`, which is what distinguishes *this did nothing
//! because it was already done* from *this did nothing and nobody noticed*.
//!
//! It also means the variables are **not** a way to reset a password: once the
//! store is closed they do nothing at all, and changing one is not a recovery
//! path. A lost owner password is still a restore from backup.
//!
//! # A store-wide owner, and not a smaller one
//!
//! The declared user has no `ON`, so it owns the store rather than one database.
//! Anything narrower cannot declare the users that come after it, and cannot
//! take a backup — `BACKUP` is the one statement whose subject is the store, so
//! a scoped owner has no grant that could reach it. A bootstrap identity that
//! cannot back the store up is a bootstrap identity somebody has to work around
//! on their first day.
//!
//! # What the environment costs, and it is not nothing
//!
//! A variable is readable by anything that can inspect the container or the
//! process, which is a smaller circle than a command line but is not a secret
//! store. It is stated here rather than left to be discovered, and reading the
//! password from a file instead — the `_FILE` convention — is the obvious next
//! step when somebody wants one.

use std::env;

use tessaridb::{Db, Error};

/// The variable naming the first user.
pub const USER: &str = "TESSARIDB_INITIAL_USER";
/// The variable carrying that user's password.
pub const PASSWORD: &str = "TESSARIDB_INITIAL_PASSWORD";

/// Declare the first user if the environment asks for one and the store has none.
///
/// Answers the name that was declared, or `None` when nothing was asked for or
/// the store already had users.
///
/// # Errors
///
/// Returns a message when only one of the two variables is set, when the name is
/// not one this can safely declare, or when the store refuses the statement for
/// any reason other than already being closed.
pub fn first_user(db: &Db) -> Result<Option<String>, String> {
    let name = env::var(USER).ok().filter(|held| !held.is_empty());
    let password = env::var(PASSWORD).ok().filter(|held| !held.is_empty());
    let (name, password) = match (name, password) {
        (None, None) => return Ok(None),
        (Some(name), Some(password)) => (name, password),
        // Half a credential is a misconfiguration, and the failure it would
        // otherwise cause is the worst kind: a node that came up **open**
        // because the password variable was misspelled, serving a network,
        // looking exactly like a node that came up correctly.
        (Some(_), None) => return Err(format!("{USER} is set but {PASSWORD} is not")),
        (None, Some(_)) => return Err(format!("{PASSWORD} is set but {USER} is not")),
    };
    if !is_plain_name(&name) {
        return Err(format!(
            "{USER} must be a plain name — letters, digits and underscores, not \
             starting with a digit — and {name:?} is not"
        ));
    }

    // Escaped rather than trusted: this is the one statement in the program built
    // out of text somebody else chose, and a password is exactly the value most
    // likely to contain a quote.
    let script = format!(
        "DEFINE USER {name} ROLE owner PASSWORD '{}';",
        quoted(&password)
    );
    let mut session = db.session();
    match session.run(&script) {
        Ok(_) => {
            log::info!("declared {name} as the store's first user, from {USER}");
            Ok(Some(name))
        }
        // The store is closed, which means it already has users. Every start
        // after the first reaches this, so it is not a failure.
        Err(Error::NotSignedIn { .. }) => {
            log::info!("this store already has users, so {USER} was not applied");
            Ok(None)
        }
        Err(failure) => Err(format!("{USER}: {failure}")),
    }
}

/// Whether this is a name that can be written into a statement as itself.
///
/// Deliberately narrower than what the language accepts. A bootstrap name is
/// chosen once by whoever deploys the node, so refusing an exotic one costs them
/// a keystroke — and the alternative is quoting rules on the one input this
/// program cannot see before it runs.
fn is_plain_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && characters.all(|held| held.is_ascii_alphanumeric() || held == '_')
}

/// A password, escaped for a single-quoted literal.
///
/// The lexer's escapes are `\\`, `\'`, `\"`, `\n`, `\t`, `\r` and `\0`, and
/// anything else after a backslash is an error rather than a literal character.
/// So a backslash has to be doubled **before** the quote is escaped, or the
/// backslash this adds would itself be doubled a moment later.
fn quoted(password: &str) -> String {
    password.replace('\\', r"\\").replace('\'', r"\'")
}

#[cfg(test)]
mod tests {
    use super::{is_plain_name, quoted};

    #[test]
    fn a_plain_name_is_letters_digits_and_underscores() {
        assert!(is_plain_name("root"));
        assert!(is_plain_name("_root"));
        assert!(is_plain_name("root_2"));
        assert!(!is_plain_name("2root"), "a name may not start with a digit");
        assert!(!is_plain_name(""), "there is no empty name");
    }

    #[test]
    fn a_name_that_could_carry_a_statement_is_refused() {
        // The reason the check exists: a name is written into the statement as
        // itself, so anything that could end it must not reach the formatter.
        assert!(!is_plain_name("root; DROP USER other"));
        assert!(!is_plain_name("root'"));
        assert!(!is_plain_name("root ON prod.shop"));
        assert!(!is_plain_name("ro ot"));
    }

    #[test]
    fn a_password_keeps_its_own_quotes_and_backslashes() {
        assert_eq!(quoted("plain"), "plain");
        assert_eq!(quoted("it's"), r"it\'s");
        assert_eq!(quoted(r"back\slash"), r"back\\slash");
    }

    #[test]
    fn the_backslash_is_doubled_before_the_quote_is_escaped() {
        // `\'` in a password must survive as a backslash followed by a quote,
        // not become an escaped quote. Escaping in the other order produces
        // `\\'`, which ends the literal early and is how this would have been a
        // way to run a statement instead of setting a password.
        assert_eq!(quoted(r"\'"), r"\\\'");
    }

    #[test]
    fn a_password_that_tries_to_end_the_statement_stays_a_password() {
        let escaped = quoted("'; DEFINE USER mallory ROLE owner PASSWORD 'x");
        let script = format!("DEFINE USER root ROLE owner PASSWORD '{escaped}';");
        // One statement, whatever the password says. The parser is the authority
        // on that, and `bootstrap.rs`'s integration test asks it; here it is
        // enough that the quote which would have ended the literal is escaped.
        assert!(script.contains(r"\'; DEFINE USER"), "{script}");
    }
}
