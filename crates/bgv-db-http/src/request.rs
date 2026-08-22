//! Reading the envelope a script arrives in.
//!
//! # Why a parameter's value is written in bgvQL
//!
//! Because this store has one value syntax and JSON is not it. JSON has six
//! types and the value system has fifteen, so a JSON→`Value` mapping is a
//! *decision* rather than a translation — and it is the decision `dec` exists to
//! prevent, arriving at the last step instead of the first: a caller writing
//! `12.34` in JSON hands over a double.
//!
//! [`crate::json`] already made the outbound half of this call, writing every
//! awkward kind as a string "that parses back through the same reader that read
//! it from a script". This is the inbound half, and it makes the same call: the
//! value is the text a script would have written, and the language reads it. The
//! CLI's `--param x=3` already works this way, so a caller who has used one
//! surface knows the other.
//!
//! So `{"n": "3"}` binds the integer three and `{"n": "'3'"}` binds the text —
//! the same distinction the CLI draws, in the same spelling.
//!
//! # Why the reader is here rather than a dependency
//!
//! It reads **one shape**, not JSON: an object with a string `script` and an
//! optional object of strings `parameters`. That is small enough to be right and
//! to be tested, and this crate's dependency list is short on purpose.
//!
//! Being a reader for one shape rather than a JSON parser, it is deliberately
//! **lenient in one place**: a raw newline inside a string is accepted, where
//! strict JSON demands `\n`. Scripts are multi-line and get pasted, and refusing
//! a body that says exactly what it means would be pedantry with no reader on
//! the other side to protect. Every escape that *is* written is read properly,
//! which is the half that could corrupt a statement.

use std::collections::BTreeMap;

/// A script and the values its parameters bind to, as written.
#[derive(Debug)]
pub(crate) struct Envelope {
    /// The script to run.
    pub script: String,
    /// Each parameter's value, written in bgvQL.
    pub parameters: BTreeMap<String, String>,
}

/// Read the envelope, or say what was wrong with it.
///
/// # Errors
///
/// Returns a sentence naming the problem, which the caller answers `400` with —
/// a malformed request is the client's mistake and it deserves to be told which
/// one.
pub(crate) fn envelope(body: &str) -> Result<Envelope, String> {
    let mut at = Reader::new(body);
    at.space();
    at.expect('{')?;
    let mut script = None;
    let mut parameters = BTreeMap::new();
    at.space();
    if !at.eat('}') {
        loop {
            at.space();
            let key = at.string()?;
            at.space();
            at.expect(':')?;
            at.space();
            match key.as_str() {
                "script" => script = Some(at.string()?),
                "parameters" => parameters = at.strings()?,
                other => return Err(format!("the envelope has no {other:?} field")),
            }
            at.space();
            if at.eat(',') {
                continue;
            }
            at.expect('}')?;
            break;
        }
    }
    at.space();
    if !at.done() {
        return Err("the envelope ends before the body does".to_owned());
    }
    Ok(Envelope {
        script: script.ok_or_else(|| "the envelope needs a `script`".to_owned())?,
        parameters,
    })
}

/// A position in the text, and the few things this envelope can hold.
struct Reader {
    held: Vec<char>,
    at: usize,
}

impl Reader {
    fn new(source: &str) -> Self {
        Self {
            held: source.chars().collect(),
            at: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.held.get(self.at).copied()
    }

    fn done(&self) -> bool {
        self.at >= self.held.len()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.at = self.at.saturating_add(1);
        }
    }

    fn eat(&mut self, wanted: char) -> bool {
        if self.peek() == Some(wanted) {
            self.at = self.at.saturating_add(1);
            return true;
        }
        false
    }

    fn expect(&mut self, wanted: char) -> Result<(), String> {
        if self.eat(wanted) {
            return Ok(());
        }
        Err(match self.peek() {
            Some(found) => format!("expected {wanted:?} and found {found:?}"),
            None => format!("expected {wanted:?} and the body ended"),
        })
    }

    /// One JSON string, escapes and all.
    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut held = String::new();
        loop {
            let Some(found) = self.peek() else {
                return Err("a string is not closed".to_owned());
            };
            self.at = self.at.saturating_add(1);
            match found {
                '"' => return Ok(held),
                '\\' => held.push(self.escape()?),
                other => held.push(other),
            }
        }
    }

    /// What follows a backslash.
    ///
    /// The six one-character escapes and `\u`. A surrogate pair is **not**
    /// assembled: a parameter's value is bgvQL text, and a caller writing an
    /// astral character can write it directly in a UTF-8 body — so an escape
    /// that cannot stand alone is refused rather than silently becoming a
    /// replacement character.
    fn escape(&mut self) -> Result<char, String> {
        let Some(found) = self.peek() else {
            return Err("an escape is not finished".to_owned());
        };
        self.at = self.at.saturating_add(1);
        Ok(match found {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let mut code = 0_u32;
                for _ in 0..4_u8 {
                    let Some(digit) = self.peek().and_then(|held| held.to_digit(16)) else {
                        return Err(r"\u wants four hexadecimal digits".to_owned());
                    };
                    self.at = self.at.saturating_add(1);
                    code = code.saturating_mul(16).saturating_add(digit);
                }
                char::from_u32(code)
                    .ok_or_else(|| format!(r"\u{code:04x} is not a character on its own"))?
            }
            other => return Err(format!("\\{other} is not an escape")),
        })
    }

    /// An object whose every value is a string.
    fn strings(&mut self) -> Result<BTreeMap<String, String>, String> {
        self.expect('{')?;
        let mut held = BTreeMap::new();
        self.space();
        if self.eat('}') {
            return Ok(held);
        }
        loop {
            self.space();
            let name = self.string()?;
            self.space();
            self.expect(':')?;
            self.space();
            let written = self.string().map_err(|reason| {
                format!("a parameter's value is written in bgvQL, as a string ({reason})")
            })?;
            held.insert(name, written);
            self.space();
            if self.eat(',') {
                continue;
            }
            self.expect('}')?;
            return Ok(held);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::envelope;

    #[test]
    fn a_script_and_its_values_come_back() {
        let read = envelope(r#"{"script":"SELECT * FROM t WHERE n = $n;","parameters":{"n":"3"}}"#)
            .unwrap();
        assert_eq!(read.script, "SELECT * FROM t WHERE n = $n;");
        assert_eq!(read.parameters.get("n").map(String::as_str), Some("3"));
    }

    #[test]
    fn the_parameters_are_optional_and_the_order_is_not_fixed() {
        assert!(
            envelope(r#"{"script":"SELECT 1;"}"#)
                .unwrap()
                .parameters
                .is_empty()
        );
        let read = envelope(r#"{ "parameters" : { } , "script" : "SELECT 1;" }"#).unwrap();
        assert_eq!(read.script, "SELECT 1;");
    }

    #[test]
    fn escapes_are_read_rather_than_passed_through() {
        // A script arrives with newlines and quotes in it, so getting this wrong
        // would corrupt the statement rather than the envelope.
        let read = envelope(r#"{"script":"CREATE t:1 = { s: 'a\"b' };\nSELECT 1;"}"#).unwrap();
        assert!(read.script.contains('\n'), "{:?}", read.script);
        assert!(read.script.contains('"'), "{:?}", read.script);
        let read = envelope(r#"{"script":"A\t\\"}"#).unwrap();
        assert_eq!(read.script, "A\t\\");
    }

    #[test]
    fn what_is_wrong_is_said_rather_than_guessed() {
        for (body, expected) in [
            ("", "expected '{'"),
            (r#"{"script":"a""#, "expected"),
            (r#"{"parameters":{}}"#, "needs a `script`"),
            (r#"{"scrpit":"a"}"#, "no \"scrpit\" field"),
            (r#"{"script":"a"} trailing"#, "ends before the body"),
            (r#"{"script":"a","parameters":{"n":3}}"#, "written in bgvQL"),
            (r#"{"script":"\q"}"#, "not an escape"),
            (r#"{"script":"\u00"}"#, "four hexadecimal"),
        ] {
            let reason = envelope(body).unwrap_err();
            assert!(
                reason.contains(expected),
                "{body:?} said {reason:?}, which does not mention {expected:?}"
            );
        }
    }
}
