//! Transaction verbs inside a simple query's string: `BEGIN; …; COMMIT` in
//! one Query message, the block the client's own.
//!
//! The store runs the verbs natively — `BEGIN` opens, the statements between
//! the verbs join the block, `COMMIT`/`CANCEL` close it — so the translation
//! is a faithful spelling: verbs to the store's own words, everything else
//! to the same translators a lone statement takes. One parser drives the
//! whole string, so a parameter a later statement binds never collides with
//! an earlier statement's name.
//!
//! The presence scan is quote-aware: a semicolon inside a string literal is
//! the literal's own character, not a statement's edge. That is the whole
//! lexical surface this build's dialect has — no line comments, no
//! dollar-quoting — and a wider scanner is the day one of those arrives.

use super::parser::Parser;
use super::{Refused, Translated, code, write};

/// Whether any statement of `query` opens with a transaction verb — the
/// routing question that sends the string here instead of the SELECT-only or
/// write-only multi-statement paths, which refuse anything else.
pub(crate) fn present(query: &str) -> bool {
    let bytes = query.as_bytes();
    let mut at = 0usize;
    loop {
        while let Some(byte) = bytes.get(at) {
            if !(byte.is_ascii_whitespace() || *byte == b';') {
                break;
            }
            at = at.saturating_add(1);
        }
        if at >= bytes.len() {
            return false;
        }
        let start = at;
        while let Some(byte) = bytes.get(at) {
            if !(byte.is_ascii_alphanumeric() || *byte == b'_') {
                break;
            }
            at = at.saturating_add(1);
        }
        if at == start {
            // No word opens here — not a verb; let the translator refuse it
            // in its own words.
            return false;
        }
        if is_verb(&query[start..at]) {
            return true;
        }
        // Skip to the next statement edge, respecting the literal quotes.
        while at < bytes.len() {
            match bytes[at] {
                b';' => break,
                b'\'' => {
                    at = at.saturating_add(1);
                    while at < bytes.len() {
                        if bytes[at] == b'\'' {
                            // A doubled quote is the literal's own character.
                            if bytes.get(at.saturating_add(1)) == Some(&b'\'') {
                                at = at.saturating_add(2);
                            } else {
                                at = at.saturating_add(1);
                                break;
                            }
                        } else {
                            at = at.saturating_add(1);
                        }
                    }
                }
                b'"' => {
                    at = at.saturating_add(1);
                    while at < bytes.len() {
                        if bytes[at] == b'"' {
                            if bytes.get(at.saturating_add(1)) == Some(&b'"') {
                                at = at.saturating_add(2);
                            } else {
                                at = at.saturating_add(1);
                                break;
                            }
                        } else {
                            at = at.saturating_add(1);
                        }
                    }
                }
                _ => at = at.saturating_add(1),
            }
        }
    }
}

/// Whether the word opening a statement is a transaction verb.
fn is_verb(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "begin" | "start" | "commit" | "end" | "rollback" | "abort"
    )
}

/// Translate a string carrying transaction verbs: every statement of it, on
/// one parser, in the order it runs. The assembled script is the store's own
/// block — no implicit wrapping, the verbs say what holds.
pub(crate) fn translate(query: &str) -> Result<Translated, Refused> {
    let mut parser = Parser::new(query);
    let mut statements = Vec::new();
    loop {
        if let Some(verb) = parser.transaction_verb()? {
            statements.push(verb);
        } else if parser.peek_word().is_some_and(|word| write::opens(&word)) {
            statements.push(write::statement(&mut parser)?);
        } else {
            if parser.keyword("SELECT").is_err() {
                return Err(Refused::surface(
                    code::SYNTAX,
                    "a query starts with SELECT in this build",
                ));
            }
            statements.push(parser.shape()?);
        }
        if !parser.end_more()? {
            break;
        }
    }
    let script = statements
        .iter()
        .map(|shaped| shaped.script.trim_end_matches(';'))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Translated {
        script: format!("{script};"),
        parameters: std::mem::take(&mut parser.parameters),
        statements,
        slots: std::mem::take(&mut parser.slots),
    })
}
