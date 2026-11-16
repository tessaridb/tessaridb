//! The session commands the dialect's clients send beside queries: the
//! namespace navigation pair. `SET search_path` moves the connection to
//! another namespace — the store's `USE NAMESPACE` under the dialect's own
//! navigation word; `SHOW search_path` answers where the connection reads.

use super::errors::{Refused, code};

/// What a session command asks of the connection.
pub(crate) enum Session {
    /// `SET search_path TO <name>` / `= <name>`: read under this namespace.
    Namespace(String),
    /// `SHOW search_path`: answer the namespace the connection reads under.
    ShowNamespace,
    /// A transaction verb: `BEGIN`/`START TRANSACTION`, `COMMIT`/`END`,
    /// `ROLLBACK`/`ABORT`.
    Transaction(TxVerb),
}

/// Which end of a transaction block a verb names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxVerb {
    /// `BEGIN` / `START TRANSACTION`.
    Begin,
    /// `COMMIT` / `END`.
    Commit,
    /// `ROLLBACK` / `ABORT`.
    Rollback,
}

/// Parse one session command; `Ok(None)` hands the query to the translator,
/// which refuses what is not a query in the store's own words.
pub(crate) fn parse(query: &str) -> Result<Option<Session>, Refused> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let Some(first) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return Ok(None);
    };
    match first.as_str() {
        "set" => set(&words[1..]).map(Some),
        "show" => show(&words[1..]).map(Some),
        "begin" => {
            if words.len() == 1 {
                return Ok(Some(Session::Transaction(TxVerb::Begin)));
            }
            Err(Refused::surface(
                code::SYNTAX,
                "BEGIN takes no words in this build",
            ))
        }
        "start" => {
            if words
                .get(1)
                .is_some_and(|word| word.eq_ignore_ascii_case("transaction"))
                && words.len() == 2
            {
                return Ok(Some(Session::Transaction(TxVerb::Begin)));
            }
            Err(Refused::surface(
                code::SYNTAX,
                "START wants TRANSACTION in this build",
            ))
        }
        "commit" | "end" => {
            if words.len() == 1 {
                return Ok(Some(Session::Transaction(TxVerb::Commit)));
            }
            Err(Refused::surface(
                code::SYNTAX,
                "COMMIT takes no words in this build",
            ))
        }
        "rollback" | "abort" => {
            if words.len() == 1 {
                return Ok(Some(Session::Transaction(TxVerb::Rollback)));
            }
            Err(Refused::surface(
                code::SYNTAX,
                "ROLLBACK takes no words in this build",
            ))
        }
        _ => Ok(None),
    }
}

fn set(words: &[&str]) -> Result<Session, Refused> {
    let Some(name) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return Err(Refused::surface(code::SYNTAX, "SET wants a setting name"));
    };
    if name != "search_path" {
        return Err(Refused::with_detail(
            code::FEATURE,
            "this session setting is not spoken in this build",
            format!("setting {name:?}"),
        ));
    }
    let Some(separator) = words.get(1) else {
        return Err(Refused::surface(
            code::SYNTAX,
            "SET search_path wants TO or =",
        ));
    };
    if !separator.eq_ignore_ascii_case("to") && *separator != "=" {
        return Err(Refused::surface(
            code::SYNTAX,
            "SET search_path wants TO or =",
        ));
    }
    let Some(value) = words.get(2) else {
        return Err(Refused::surface(
            code::SYNTAX,
            "SET search_path wants a name",
        ));
    };
    if words.len() > 3 {
        return Err(Refused::surface(
            code::SYNTAX,
            "SET search_path takes one name in this build",
        ));
    }
    Ok(Session::Namespace(name_of(value)?))
}

fn show(words: &[&str]) -> Result<Session, Refused> {
    let Some(name) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return Err(Refused::surface(code::SYNTAX, "SHOW wants a setting name"));
    };
    if words.len() > 1 {
        return Err(Refused::surface(code::SYNTAX, "SHOW takes one name"));
    }
    if name != "search_path" {
        return Err(Refused::with_detail(
            code::FEATURE,
            "this session setting is not spoken in this build",
            format!("setting {name:?}"),
        ));
    }
    Ok(Session::ShowNamespace)
}

/// One bare name, checked the way the translator checks one: lowercase,
/// letters digits and underscores, no quotes — the store's lexer has bare
/// words alone.
fn name_of(word: &str) -> Result<String, Refused> {
    if word.starts_with('"') {
        return Err(Refused::surface(
            code::FEATURE,
            "quoted identifiers are not spoken in this build",
        ));
    }
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return Err(Refused::surface(code::SYNTAX, "a name was expected"));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(Refused::surface(code::SYNTAX, "a name was expected"));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Refused::surface(code::SYNTAX, "a name was expected"));
    }
    Ok(word.to_ascii_lowercase())
}
