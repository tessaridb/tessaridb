//! The SQL this surface speaks, translated to the store's own statement text
//! with every literal bound as a parameter.
//!
//! One shape for now: `SELECT` over one table with a boolean condition and a
//! limit — the read every BI tool starts from. Everything the dialect allows
//! but this build does not speak is refused by name (code `0A000`), never
//! approximated: a client sees "not spoken" and knows what it is holding.

use tessaridb::{Parameters, Value};

use super::errors::{Refused, code};

/// What the translator owes the caller: the script, its bound values, and the
/// column order the projection named (or the mark that it was `*`). `slots`
/// maps a `$n` placeholder to the parameter name the Bind's nth value takes —
/// `None` marks a slot the query never used, which Bind refuses.
#[derive(Debug, Clone)]
pub(crate) struct Translated {
    pub(crate) script: String,
    pub(crate) parameters: Parameters,
    pub(crate) columns: Columns,
    /// The table the FROM named, for the identity column's spelling.
    pub(crate) table: Option<String>,
    /// The parameter name each `$1..$n` takes, in placeholder order.
    pub(crate) slots: Vec<Option<String>>,
}

/// The projection's shape: the names it answers under, in order, or a star
/// whose names the answer itself must supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Columns {
    Named(Vec<String>),
    Star,
}

/// The name the dialect gives a column the projection did not name.
pub(crate) const UNNAMED: &str = "?column?";

/// Translate one query string. A refusal carries the wire code and the words
/// a client shows.
pub(crate) fn translate(query: &str) -> Result<Translated, Refused> {
    let parser = Parser::new(query);
    let select = parser.select()?;
    Ok(select)
}

/// The parser state: the source, a cursor over it, and the bound values the
/// script collects as it goes.
struct Parser<'a> {
    source: &'a [u8],
    at: usize,
    parameters: Parameters,
    next: usize,
    slots: Vec<Option<String>>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source: source.as_bytes(),
            at: 0,
            parameters: Parameters::new(),
            next: 0,
            slots: Vec::new(),
        }
    }

    /// One bound parameter, named and kept.
    fn bind(&mut self, value: Value) -> String {
        self.next = self.next.saturating_add(1);
        let name = format!("p{}", self.next);
        self.parameters.insert(name.clone(), value);
        format!("${name}")
    }

    fn select(mut self) -> Result<Translated, Refused> {
        if self.keyword("SELECT").is_err() {
            return Err(Refused::surface(
                code::SYNTAX,
                "a query starts with SELECT in this build",
            ));
        }
        let projection = self.projection()?;
        let mut from = None;
        let mut alias = None;
        if self.keyword("FROM").is_ok() {
            from = Some(self.ident()?);
            if self.keyword("AS").is_ok() {
                alias = Some(self.ident()?);
            } else {
                alias = self.bare_alias();
            }
            let at = self.at;
            if self.dot() {
                return Err(Refused::surface(
                    code::FEATURE,
                    "one table is read at a time in this build; a qualified name is not spoken",
                ));
            }
            self.at = at;
        }
        let condition = if self.keyword("WHERE").is_ok() {
            Some(self.or()?)
        } else {
            None
        };
        let limit = if self.keyword("LIMIT").is_ok() {
            Some(self.number()?)
        } else {
            None
        };
        if self.keyword("OFFSET").is_ok() {
            return Err(Refused::surface(
                code::FEATURE,
                "OFFSET is not spoken in this build",
            ));
        }
        self.end()?;

        let (script, columns) = match &from {
            Some(table) => {
                let mut script = String::from("SELECT ");
                script.push_str(&projection.text);
                script.push_str(" FROM ");
                script.push_str(table);
                if let Some(alias) = &alias {
                    script.push_str(" AS ");
                    script.push_str(alias);
                }
                if let Some(condition) = condition {
                    script.push_str(" WHERE ");
                    script.push_str(&condition);
                }
                if let Some(limit) = limit {
                    script.push_str(&format!(" LIMIT {limit}"));
                }
                script.push(';');
                (script, projection.columns)
            }
            None => {
                if condition.is_some() || limit.is_some() {
                    return Err(Refused::surface(
                        code::SYNTAX,
                        "WHERE and LIMIT want a FROM in this build",
                    ));
                }
                let mut script = String::from("RETURN ");
                script.push_str(&projection.text);
                script.push(';');
                (script, Columns::Named(vec![UNNAMED.to_owned()]))
            }
        };
        Ok(Translated {
            script,
            parameters: self.parameters,
            columns,
            table: from,
            slots: self.slots,
        })
    }

    /// A `$n` placeholder at the cursor: the script keeps a named parameter
    /// the Bind's nth value fills. Every use of the same `$n` takes one name.
    fn placeholder(&mut self) -> Option<String> {
        let at = self.at;
        self.skip_space();
        if !self.byte(b'$') {
            self.at = at;
            return None;
        }
        let digits = self.digits_raw();
        let Ok(index) = digits.parse::<usize>() else {
            self.at = at;
            return None;
        };
        if index == 0 {
            self.at = at;
            return None;
        }
        while self.slots.len() < index {
            self.slots.push(None);
        }
        if let Some(name) = &self.slots[index.saturating_sub(1)] {
            return Some(format!("${name}"));
        }
        let name = format!("e{index}");
        self.slots[index.saturating_sub(1)] = Some(name.clone());
        Some(format!("${name}"))
    }

    /// `*` or a comma list of fields and literals, each with its alias.
    fn projection(&mut self) -> Result<Projection, Refused> {
        if self.star() {
            return Ok(Projection {
                text: "*".to_owned(),
                columns: Columns::Star,
            });
        }
        let mut text = String::new();
        let mut columns = Vec::new();
        loop {
            let item = self.item()?;
            text.push_str(&item.text);
            columns.push(item.column);
            if self.comma().is_err() {
                break;
            }
            text.push_str(", ");
        }
        Ok(Projection {
            text,
            columns: Columns::Named(columns),
        })
    }

    /// One projection item: a field path or a literal, with an optional alias.
    fn item(&mut self) -> Result<Item, Refused> {
        if let Some(held) = self.placeholder() {
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{held} AS {alias}"), alias),
                None => (held, UNNAMED.to_owned()),
            };
            return Ok(Item { text, column });
        }
        if let Some(literal) = self.literal()? {
            let bound = self.bind(literal);
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{bound} AS {alias}"), alias),
                None => (bound, UNNAMED.to_owned()),
            };
            return Ok(Item { text, column });
        }
        let path = self.ident_path()?;
        let (text, column) = match self.alias()? {
            Some(alias) => (format!("{path} AS {alias}"), alias),
            None => (
                path.clone(),
                path.rsplit('.').next().unwrap_or(&path).to_owned(),
            ),
        };
        Ok(Item { text, column })
    }

    fn alias(&mut self) -> Result<Option<String>, Refused> {
        if self.keyword("AS").is_ok() {
            return Ok(Some(self.ident()?));
        }
        Ok(self.bare_alias())
    }

    /// A bare alias: the word after the item, unless it is a keyword of the
    /// shape itself — `SELECT name FROM …` has no alias on `name`, and
    /// `FROM orders WHERE …` has none on the table.
    fn bare_alias(&mut self) -> Option<String> {
        let at = self.at;
        let word = self.bare_ident()?;
        if is_keyword(&word) {
            self.at = at;
            return None;
        }
        Some(word)
    }

    /// A boolean expression: `or` of `and`s of `not`s.
    fn or(&mut self) -> Result<String, Refused> {
        let mut text = self.and()?;
        while self.keyword("OR").is_ok() {
            text.push_str(" OR ");
            text.push_str(&self.and()?);
        }
        Ok(text)
    }

    fn and(&mut self) -> Result<String, Refused> {
        let mut text = self.not()?;
        while self.keyword("AND").is_ok() {
            text.push_str(" AND ");
            text.push_str(&self.not()?);
        }
        Ok(text)
    }

    fn not(&mut self) -> Result<String, Refused> {
        if self.keyword("NOT").is_ok() {
            return Ok(format!("NOT {}", self.not()?));
        }
        self.predicate()
    }

    /// A comparison, an `IS [NOT] NULL`, or a parenthesized expression.
    fn predicate(&mut self) -> Result<String, Refused> {
        if self.open_paren() {
            let inner = self.or()?;
            self.close_paren()?;
            return Ok(format!("({inner})"));
        }
        let left = self.operand()?;
        if self.keyword("IS").is_ok() {
            let negated = self.keyword("NOT").is_ok();
            self.keyword("NULL")
                .map_err(|_| Refused::surface(code::SYNTAX, "IS wants NULL in this build"))?;
            // The dialect's one NULL is the store's two absences: present and
            // holding nothing, or not present at all.
            return Ok(if negated {
                format!("({left} != NULL AND {left} != NONE)")
            } else {
                format!("({left} = NULL OR {left} = NONE)")
            });
        }
        for operator in ["!=", "<=", ">=", "=", "<", ">"] {
            if self.operator(operator) {
                let right = self.operand()?;
                return Ok(format!("{left} {operator} {right}"));
            }
        }
        Err(Refused::with_detail(
            code::SYNTAX,
            "a condition wants a comparison",
            format!("after {left:?}"),
        ))
    }

    /// One side of a comparison: a placeholder passes as its parameter; a
    /// literal binds; a field path passes.
    fn operand(&mut self) -> Result<String, Refused> {
        if let Some(held) = self.placeholder() {
            return Ok(held);
        }
        if let Some(literal) = self.literal()? {
            return Ok(self.bind(literal));
        }
        self.ident_path()
    }

    /// A literal of the dialect: a `$n` placeholder, a number, a string, a
    /// boolean, or NULL.
    fn literal(&mut self) -> Result<Option<Value>, Refused> {
        if let Some(number) = self.numeric_literal() {
            return Ok(Some(number));
        }
        if let Some(text) = self.string_literal() {
            return Ok(Some(Value::String(text)));
        }
        if self.keyword("TRUE").is_ok() {
            return Ok(Some(Value::Bool(true)));
        }
        if self.keyword("FALSE").is_ok() {
            return Ok(Some(Value::Bool(false)));
        }
        if self.keyword("NULL").is_ok() {
            return Ok(Some(Value::Null));
        }
        Ok(None)
    }

    /// A number is one contiguous token: sign, digits, one dot, digits. Every
    /// failure puts the cursor back — a probe is non-consuming on failure.
    fn numeric_literal(&mut self) -> Option<Value> {
        self.skip_space();
        let start = self.at;
        let _negative = self.byte(b'-');
        let integer = self.digits_raw();
        let mut dotted = false;
        let mut fraction = String::new();
        if self.byte(b'.') {
            dotted = true;
            fraction = self.digits_raw();
        }
        if integer.is_empty() && (!dotted || fraction.is_empty()) {
            self.at = start;
            return None;
        }
        let word = self.slice(start);
        let parsed = if dotted {
            word.parse::<f64>().ok().map(tessaridb::Number::float)
        } else {
            word.parse::<i64>().ok().map(Into::into)
        };
        let Some(number) = parsed else {
            self.at = start;
            return None;
        };
        Some(Value::Number(number))
    }

    /// Digits at the cursor, contiguous; the caller owns spacing.
    fn digits_raw(&mut self) -> String {
        let start = self.at;
        while let Some(byte) = self.source.get(self.at) {
            if !byte.is_ascii_digit() {
                break;
            }
            self.at = self.at.saturating_add(1);
        }
        self.slice(start).to_owned()
    }

    fn string_literal(&mut self) -> Option<String> {
        if !self.quote() {
            return None;
        }
        let mut out = String::new();
        loop {
            let rest = &self.source[self.at..];
            let end = rest.iter().position(|b| *b == b'\'')?;
            out.push_str(&String::from_utf8_lossy(&rest[..end]));
            self.at = self.at.saturating_add(end).saturating_add(1);
            // Two quotes are the dialect's one escaped quote.
            if self.quote() {
                out.push('\'');
            } else {
                return Some(out);
            }
        }
    }

    /// A dotted field path, each part checked as a name.
    fn ident_path(&mut self) -> Result<String, Refused> {
        let mut path = self.ident()?;
        while self.dot() {
            path.push('.');
            path.push_str(&self.ident()?);
        }
        Ok(path)
    }

    /// One name: a bare word folded lowercase. Names are grammar — they are
    /// checked, never bound. The store's own language names its tables and
    /// fields in bare words alone, so a quoted identifier is refused by name
    /// rather than silently de-quoted, and a keyword is not a name.
    fn ident(&mut self) -> Result<String, Refused> {
        if self.peek_quoted() {
            return Err(Refused::surface(
                code::FEATURE,
                "quoted identifiers are not spoken in this build",
            ));
        }
        let at = self.at;
        let word = self.bare_ident().ok_or_else(|| {
            Refused::with_detail(
                code::SYNTAX,
                "a name was expected",
                format!("at byte {}", self.at),
            )
        })?;
        if is_keyword(&word) {
            self.at = at;
            return Err(Refused::surface(code::SYNTAX, "a name was expected"));
        }
        Ok(word)
    }

    /// Whether a double quote sits at the cursor, without consuming it.
    fn peek_quoted(&mut self) -> bool {
        let at = self.at;
        self.skip_space();
        let quoted = self.source.get(self.at) == Some(&b'"');
        self.at = at;
        quoted
    }

    /// A bare word at the cursor, folded: what SQL spells without quotes.
    fn bare_ident(&mut self) -> Option<String> {
        self.skip_space();
        let word = self.bare_ident_raw()?;
        Some(word.to_ascii_lowercase())
    }

    /// A whole number for LIMIT.
    fn number(&mut self) -> Result<u64, Refused> {
        let word = self.digits();
        if word.is_empty() {
            return Err(Refused::surface(code::SYNTAX, "a number was expected"));
        }
        word.parse::<u64>()
            .map_err(|_| Refused::surface(code::SYNTAX, "the number does not fit"))
    }

    // The lexical atoms, each leaving the cursor past what it ate.

    fn keyword(&mut self, word: &str) -> Result<(), ()> {
        self.skip_space();
        let start = self.at;
        let Some(bare) = self.bare_ident_raw() else {
            return Err(());
        };
        if bare.eq_ignore_ascii_case(word) {
            Ok(())
        } else {
            self.at = start;
            Err(())
        }
    }

    fn operator(&mut self, mark: &str) -> bool {
        let at = self.at;
        self.skip_space();
        if self.source[self.at..].starts_with(mark.as_bytes()) {
            self.at = self.at.saturating_add(mark.len());
            true
        } else {
            // A failed probe puts the cursor back: every probe is
            // non-consuming on failure, so backtracking is free.
            self.at = at;
            false
        }
    }

    fn comma(&mut self) -> Result<(), ()> {
        self.eat(b',')
    }

    fn open_paren(&mut self) -> bool {
        self.eat(b'(').is_ok()
    }

    fn close_paren(&mut self) -> Result<(), Refused> {
        self.eat(b')')
            .map_err(|_| Refused::surface(code::SYNTAX, "a closing parenthesis is missing"))
    }

    fn star(&mut self) -> bool {
        self.eat(b'*').is_ok()
    }

    fn quote(&mut self) -> bool {
        self.eat(b'\'').is_ok()
    }

    fn dot(&mut self) -> bool {
        self.eat(b'.').is_ok()
    }

    fn byte(&mut self, want: u8) -> bool {
        if self.source.get(self.at) == Some(&want) {
            self.at = self.at.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn eat(&mut self, want: u8) -> Result<(), ()> {
        let at = self.at;
        self.skip_space();
        if self.byte(want) {
            Ok(())
        } else {
            self.at = at;
            Err(())
        }
    }

    fn digits(&mut self) -> String {
        self.skip_space();
        let start = self.at;
        while let Some(byte) = self.source.get(self.at) {
            if !byte.is_ascii_digit() {
                break;
            }
            self.at = self.at.saturating_add(1);
        }
        self.slice(start).to_owned()
    }

    /// The bare word at the cursor, when it is one; used by [`Self::keyword`]
    /// and the alias path, which decide what the word means.
    fn bare_ident_raw(&mut self) -> Option<String> {
        let start = self.at;
        let first = *self.source.get(self.at)?;
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return None;
        }
        self.at = self.at.saturating_add(1);
        while let Some(byte) = self.source.get(self.at) {
            if !(byte.is_ascii_alphanumeric() || *byte == b'_') {
                break;
            }
            self.at = self.at.saturating_add(1);
        }
        Some(self.slice(start).to_owned())
    }

    fn skip_space(&mut self) {
        while let Some(byte) = self.source.get(self.at) {
            if !byte.is_ascii_whitespace() {
                break;
            }
            self.at = self.at.saturating_add(1);
        }
    }

    fn slice(&self, start: usize) -> &str {
        std::str::from_utf8(&self.source[start..self.at]).unwrap_or("")
    }

    /// The query must end here; a trailing semicolon or whitespace is the
    /// dialect's own punctuation and is accepted. What follows a semicolon
    /// that is not punctuation is another statement, which this build does
    /// not speak — the implicit transaction it would need is not built yet.
    fn end(&mut self) -> Result<(), Refused> {
        self.skip_space();
        if self.byte(b';') {
            self.skip_space();
            if self.at != self.source.len() {
                return Err(Refused::surface(
                    code::FEATURE,
                    "multiple statements in one query are not spoken in this build",
                ));
            }
            return Ok(());
        }
        if self.at == self.source.len() {
            Ok(())
        } else {
            Err(Refused::with_detail(
                code::SYNTAX,
                "syntax error at or near the rest of the query",
                format!(
                    "at byte {}: {:?}",
                    self.at,
                    String::from_utf8_lossy(&self.source[self.at..])
                ),
            ))
        }
    }
}

/// The words a bare alias may not be: the shape's own keywords. `AS` is not
/// here because the `AS` path consumed it already.
fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "select"
            | "from"
            | "where"
            | "limit"
            | "offset"
            | "and"
            | "or"
            | "not"
            | "is"
            | "null"
            | "true"
            | "false"
    )
}

/// One projection item, rendered and named.
struct Projection {
    text: String,
    columns: Columns,
}

struct Item {
    text: String,
    column: String,
}

#[cfg(test)]
mod tests;
