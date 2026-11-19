//! The session commands the dialect's clients send beside queries: the
//! namespace navigation pair and the reported-parameter surface. `SET
//! search_path` moves the connection to another namespace — the store's
//! `USE NAMESPACE` under the dialect's own navigation word; every other
//! reported parameter is accept-and-report: the connection holds the value,
//! answers `SHOW` from it, echoes a change in `ParameterStatus`, and only
//! `search_path` changes what the store reads. Values the port cannot
//! honestly perform — another client_encoding, another zone, a read-only
//! parameter — refuse by name rather than pretend.

use super::errors::{Refused, code};

/// What a session command asks of the connection.
pub(crate) enum Session {
    /// `SET search_path TO <name>` / `= <name>`: read under this namespace.
    Namespace(String),
    /// `SHOW search_path`: answer the namespace the connection reads under.
    ShowNamespace,
    /// `SET <name> TO <value>` for a reported, settable parameter: hold and
    /// report the value — the guards already ran at parse time.
    Set { name: String, value: String },
    /// `SHOW <name>`: answer the held value of a reported parameter.
    Show { name: String },
    /// `SHOW ALL`: every held parameter as rows.
    ShowAll,
    /// `RESET <name>` / `SET <name> TO DEFAULT`: back to the reported default.
    Reset { name: String },
    /// `RESET ALL`: every parameter back to its reported default.
    ResetAll,
    /// A transaction verb: `BEGIN`/`START TRANSACTION`, `COMMIT`/`END`,
    /// `ROLLBACK`/`ABORT`.
    Transaction(TxVerb),
}

/// Which end of a transaction block a verb names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TxVerb {
    /// `BEGIN` / `START TRANSACTION`.
    Begin,
    /// `COMMIT` / `END`.
    Commit,
    /// `ROLLBACK` / `ABORT`.
    Rollback,
    /// `SAVEPOINT name` — a named mark inside the open block.
    Savepoint {
        /// The mark's name.
        name: String,
    },
    /// `ROLLBACK TO [SAVEPOINT] name` — undo to the mark, keep the block.
    RollbackTo {
        /// The mark's name.
        name: String,
    },
    /// `RELEASE [SAVEPOINT] name` — drop the mark, keep its work.
    Release {
        /// The mark's name.
        name: String,
    },
}

/// Parse one session command; `Ok(None)` hands the query to the translator,
/// which refuses what is not a query in the store's own words. A trailing
/// semicolon is the dialect's own punctuation and says nothing — `BEGIN;`
/// is the verb `BEGIN`.
pub(crate) fn parse(query: &str) -> Result<Option<Session>, Refused> {
    let query = query.trim_end_matches([';', ' ']);
    let words: Vec<&str> = query.split_whitespace().collect();
    let Some(first) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return Ok(None);
    };
    match first.as_str() {
        "set" => set(query, &words).map(Some),
        "show" => show(&words).map(Some),
        "reset" => reset(&words).map(Some),
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
            // `ROLLBACK TO [SAVEPOINT] name`: the block stays open, the work
            // rides back to the mark. Only the `TO` spelling is the verb's;
            // a bare `ROLLBACK name` is a mistake the refusal names.
            if words
                .get(1)
                .is_some_and(|word| word.eq_ignore_ascii_case("to"))
            {
                let name = match words.len() {
                    3 => name_of(words[2]),
                    4 if words[2].eq_ignore_ascii_case("savepoint") => name_of(words[3]),
                    _ => {
                        return Err(Refused::surface(
                            code::SYNTAX,
                            "ROLLBACK TO wants a savepoint name",
                        ));
                    }
                }?;
                return Ok(Some(Session::Transaction(TxVerb::RollbackTo { name })));
            }
            Err(Refused::surface(
                code::SYNTAX,
                "ROLLBACK takes no words, or TO and a savepoint name",
            ))
        }
        "release" => {
            let name = match words.len() {
                2 => name_of(words[1]),
                3 if words[1].eq_ignore_ascii_case("savepoint") => name_of(words[2]),
                _ => {
                    return Err(Refused::surface(
                        code::SYNTAX,
                        "RELEASE wants a savepoint name",
                    ));
                }
            }?;
            Ok(Some(Session::Transaction(TxVerb::Release { name })))
        }
        "savepoint" => {
            if words.len() == 2 {
                return Ok(Some(Session::Transaction(TxVerb::Savepoint {
                    name: name_of(words[1])?,
                })));
            }
            Err(Refused::surface(code::SYNTAX, "SAVEPOINT wants one name"))
        }
        _ => Ok(None),
    }
}

/// `SET [SESSION] <name> [TO|=] <value>` — SESSION is the default and says
/// nothing; LOCAL is not spoken (its transaction-scoped effect would die at
/// the block end, unobserved). `SET <name> TO DEFAULT` is RESET. TIME ZONE
/// and NAMES are the dialect's own spellings for two reported parameters.
fn set(query: &str, words: &[&str]) -> Result<Session, Refused> {
    let mut at = 1;
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("local"))
    {
        return Err(Refused::surface(
            code::FEATURE,
            "SET LOCAL is not spoken in this build",
        ));
    }
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("session"))
    {
        at = at.saturating_add(1);
    }
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("time"))
    {
        if words
            .get(at.saturating_add(1))
            .is_some_and(|word| word.eq_ignore_ascii_case("zone"))
        {
            // The dialect's own spelling carries no TO/= separator: the zone
            // name is the rest of the text.
            at = at.saturating_add(2);
            return set_value(query, words, at, "timezone", false);
        }
        return Err(Refused::surface(code::SYNTAX, "TIME wants ZONE"));
    }
    if words
        .get(at)
        .is_some_and(|word| word.eq_ignore_ascii_case("names"))
    {
        at = at.saturating_add(1);
        return set_value(query, words, at, "client_encoding", false);
    }
    let Some(name) = words.get(at).map(|word| word.to_ascii_lowercase()) else {
        return Err(Refused::surface(code::SYNTAX, "SET wants a setting name"));
    };
    if name == "search_path" {
        // The one SET with an effect: the connection moves namespaces.
        let Some(separator) = words.get(at.saturating_add(1)) else {
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
        let value = value_after(query, at.saturating_add(1))?;
        if value.eq_ignore_ascii_case("default") {
            return Ok(Session::Reset {
                name: "search_path".to_owned(),
            });
        }
        return Ok(Session::Namespace(name_of(&value)?));
    }
    set_value(query, words, at.saturating_add(1), &name, true)
}

/// The value half of a SET: when `separator` holds, a TO/= word rides before
/// the value; TIME ZONE and NAMES carry none. Then the guards for the named
/// parameter — accepted values return canonical; anything the port cannot
/// honestly perform refuses by name.
fn set_value(
    query: &str,
    words: &[&str],
    at: usize,
    name: &str,
    separator: bool,
) -> Result<Session, Refused> {
    let value = if separator {
        let Some(separator) = words.get(at) else {
            return Err(Refused::surface(code::SYNTAX, "SET wants TO or ="));
        };
        if !separator.eq_ignore_ascii_case("to") && *separator != "=" {
            return Err(Refused::surface(code::SYNTAX, "SET wants TO or ="));
        }
        value_after(query, at)?
    } else {
        value_after(query, at.saturating_sub(1))?
    };
    if value.eq_ignore_ascii_case("default") {
        return Ok(Session::Reset {
            name: name.to_owned(),
        });
    }
    let held = match name {
        "client_encoding" => {
            if value.eq_ignore_ascii_case("utf8") || value.eq_ignore_ascii_case("utf-8") {
                "UTF8"
            } else {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    "this build speaks UTF8 alone",
                    format!("client_encoding {value:?}"),
                ));
            }
        }
        "timezone" => {
            if value.eq_ignore_ascii_case("utc") {
                "UTC"
            } else {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    "this build keeps one zone",
                    format!("TimeZone {value:?}"),
                ));
            }
        }
        "datestyle" => {
            let folded = value.to_ascii_lowercase().replace([' ', ','], "");
            if folded == "iso" {
                "ISO"
            } else if folded == "isomdy" {
                "ISO, MDY"
            } else {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    "this build renders dates the ISO way",
                    format!("DateStyle {value:?}"),
                ));
            }
        }
        "standard_conforming_strings" => {
            if value.eq_ignore_ascii_case("on") {
                "on"
            } else {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    "the string lexing always conforms in this build",
                    format!("standard_conforming_strings {value:?}"),
                ));
            }
        }
        "application_name" => {
            // Any string the client names itself by, at the dialect's length.
            // The truncation is the dialect's own rule (a name is < 64 chars).
            value.get(..63).unwrap_or(&value)
        }
        "extra_float_digits" => {
            if value.parse::<i64>().is_ok() {
                value.as_str()
            } else {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    "extra_float_digits wants an integer",
                    format!("{value:?}"),
                ));
            }
        }
        "server_version"
        | "server_encoding"
        | "integer_datetimes"
        | "is_superuser"
        | "in_hot_standby"
        | "default_transaction_read_only" => {
            return Err(Refused::with_detail(
                code::FEATURE,
                "this session parameter is read-only in this build",
                format!("setting {name:?}"),
            ));
        }
        _ => {
            return Err(Refused::with_detail(
                code::FEATURE,
                "this session setting is not spoken in this build",
                format!("setting {name:?}"),
            ));
        }
    };
    Ok(Session::Set {
        name: canonical(name),
        value: held.to_owned(),
    })
}

/// `SHOW <name>` / `SHOW ALL`: the reported parameters answer from the
/// connection's hold; what was never reported refuses by name.
fn show(words: &[&str]) -> Result<Session, Refused> {
    let Some(name) = words.get(1).map(|word| word.to_ascii_lowercase()) else {
        return Err(Refused::surface(code::SYNTAX, "SHOW wants a setting name"));
    };
    if name == "all" && words.len() == 2 {
        return Ok(Session::ShowAll);
    }
    if words.len() != 2 {
        return Err(Refused::surface(code::SYNTAX, "SHOW takes one name"));
    }
    if name == "search_path" {
        return Ok(Session::ShowNamespace);
    }
    if is_reported(&name) {
        return Ok(Session::Show {
            name: canonical(&name),
        });
    }
    Err(Refused::with_detail(
        code::FEATURE,
        "this session setting is not spoken in this build",
        format!("setting {name:?}"),
    ))
}

/// `RESET <name>` / `RESET ALL`: back to the startup-reported values.
fn reset(words: &[&str]) -> Result<Session, Refused> {
    let Some(name) = words.get(1).map(|word| word.to_ascii_lowercase()) else {
        return Err(Refused::surface(code::SYNTAX, "RESET wants a setting name"));
    };
    if words.len() != 2 {
        return Err(Refused::surface(code::SYNTAX, "RESET takes one name"));
    }
    if name == "all" {
        return Ok(Session::ResetAll);
    }
    if name == "search_path" || is_reported(&name) {
        return Ok(Session::Reset {
            name: canonical(&name),
        });
    }
    Err(Refused::with_detail(
        code::FEATURE,
        "this session setting is not spoken in this build",
        format!("setting {name:?}"),
    ))
}

/// Whether the name is one the greeting reported — what SHOW owes an answer.
fn is_reported(name: &str) -> bool {
    matches!(
        name,
        "server_version"
            | "server_encoding"
            | "client_encoding"
            | "datestyle"
            | "timezone"
            | "integer_datetimes"
            | "standard_conforming_strings"
            | "default_transaction_read_only"
            | "in_hot_standby"
            | "is_superuser"
            | "application_name"
            | "extra_float_digits"
    )
}

/// The name the connection's parameter hold answers under — the greeting's
/// own spellings, which are the ones the client saw in `ParameterStatus`.
fn canonical(name: &str) -> String {
    match name {
        "datestyle" => "DateStyle".to_owned(),
        "timezone" => "TimeZone".to_owned(),
        _ => name.to_owned(),
    }
}

/// The raw text after word `at`, whitespace-trimmed, one layer of the
/// dialect's single quotes stripped. Values may hold spaces inside one pair
/// of quotes, which word-splitting would break apart.
fn value_after(query: &str, at: usize) -> Result<String, Refused> {
    let mut rest = query;
    for _ in 0..=at {
        let Some(gap) = rest.find(char::is_whitespace) else {
            return Err(Refused::surface(code::SYNTAX, "SET wants a value"));
        };
        rest = rest[gap..].trim_start();
    }
    if rest.is_empty() {
        return Err(Refused::surface(code::SYNTAX, "SET wants a value"));
    }
    let stripped = rest
        .strip_prefix('\'')
        .and_then(|held| held.strip_suffix('\''));
    Ok(stripped.unwrap_or(rest).to_owned())
}

/// One bare name, checked the way the translator checks one: lowercase,
/// letters digits and underscores, no quotes — the store's lexer has bare
/// words alone.
fn name_of(word: &str) -> Result<String, Refused> {
    if word.starts_with('"') || word.starts_with('\'') {
        return Err(Refused::surface(
            code::FEATURE,
            "a quoted savepoint name is not spoken in this build",
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
