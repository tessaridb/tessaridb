//! The parser state: the source, a cursor over it, and the bound values the
//! script collects as it gone — plus the value-level probes: literals,
//! placeholders, aggregates, and the lexical atoms every clause shares.

use tessaridb::{Parameters, Value};

use super::{Refused, code};

/// The parser state: the source, a cursor over it, and the bound values the
/// script collects as it goes.
pub(crate) struct Parser<'a> {
    pub(crate) source: &'a [u8],
    pub(crate) at: usize,
    pub(crate) parameters: Parameters,
    next: usize,
    /// The alias counter for unnamed arithmetic projections.
    pub(crate) next_expr: usize,
    pub(crate) slots: Vec<Option<String>>,
    /// The parameter inference the statement collected: each `$n` a
    /// `column = $n` comparison names, with the column it compares against —
    /// what Parse-time type inference asks the table catalog about.
    pub(crate) inference: Vec<(usize, String)>,
    /// Subquery nesting guard: the wire's frame ceiling bounds the text, and
    /// this bounds the recursion the text could drive.
    pub(crate) depth: usize,
    /// Literals normally bind as parameters. Inside `HAVING` they render
    /// inline: the store's grouped read admits only group keys and folds, and
    /// a bound parameter reads to that check as neither — the birth site is
    /// the store's checker, and this surface does not reach it.
    pub(crate) inline_literals: bool,
}

/// The deepest subquery nesting this parser reads before refusing by name.
const MAX_DEPTH: usize = 64;

/// The store lexer's first character of a bare name — the grammar a quoted
/// identifier must fit while the store's lexer speaks bare words alone.
const fn store_name_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

/// The store lexer's continuation character of a bare name.
const fn store_name_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

impl<'a> Parser<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self {
            source: source.as_bytes(),
            at: 0,
            parameters: Parameters::new(),
            next: 0,
            next_expr: 0,
            slots: Vec::new(),
            inference: Vec::new(),
            depth: 0,
            inline_literals: false,
        }
    }

    pub(crate) fn depth_guard(&mut self) -> Result<(), Refused> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_DEPTH {
            return Err(Refused::surface(
                code::FEATURE,
                "subqueries nested this deep are not spoken in this build",
            ));
        }
        Ok(())
    }

    /// The cursor, for a caller that must look ahead and put it back.
    pub(crate) fn snapshot(&self) -> usize {
        self.at
    }

    /// Put the cursor back where [`Self::snapshot`] held it.
    pub(crate) fn restore(&mut self, at: usize) {
        self.at = at;
    }

    /// How many placeholders the statement took.
    pub(crate) fn slots_count(&self) -> usize {
        self.slots.len()
    }

    /// One bound parameter, named and kept.
    pub(crate) fn bind(&mut self, value: Value) -> String {
        self.next = self.next.saturating_add(1);
        let name = format!("p{}", self.next);
        self.parameters.insert(name.clone(), value);
        format!("${name}")
    }

    /// A literal of the dialect: a `$n` placeholder, a number, a string, a
    /// boolean, or NULL.
    pub(crate) fn literal(&mut self) -> Result<Option<Value>, Refused> {
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

    fn string_literal(&mut self) -> Option<String> {
        if !self.quote() {
            return None;
        }
        let mut out = String::new();
        loop {
            let rest = &self.source[self.at..];
            let end = rest.iter().position(|b| *b == b'\'')?;
            out.push_str(&String::from_utf8_lossy(&rest[..end]));
            let after = self.at.saturating_add(end).saturating_add(1);
            if self.source.get(after) == Some(&b'\'') {
                // Two quotes are the dialect's one escaped quote; the pair
                // is consumed together and one quote carries on.
                self.at = after.saturating_add(1);
                out.push('\'');
            } else {
                // A quote nothing doubles closes — `''` is the empty
                // string, exactly as the dialect lexes it.
                self.at = after;
                return Some(out);
            }
        }
    }

    /// A `$n` placeholder at the cursor: the script keeps a named parameter
    /// the Bind's nth value fills. Every use of the same `$n` takes one name.
    pub(crate) fn placeholder(&mut self) -> Option<String> {
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

    /// The 0-based slot index a placeholder name stands for, when the name
    /// is one this parser minted (`e{n}` from `$n+1`).
    pub(crate) fn slot_of(name: &str) -> Option<usize> {
        name.trim_start_matches('$')
            .trim_start_matches('e')
            .parse::<usize>()
            .ok()
            .map(|index| index.saturating_sub(1))
    }

    /// A double-quoted identifier at the cursor: the dialect's
    /// case-preserving spelling. The contents read with the dialect's one
    /// escape (two quotes are one), then ride verbatim into the store script
    /// — the store names its identifiers in bare words and preserves case, so
    /// a quoted name reaches a mixed-case table exactly. Contents the store's
    /// own name grammar cannot hold refuse by name: pretending them bare
    /// would change the name, and the store's lexer learning quoted names is
    /// engine work, not this layer's.
    pub(crate) fn quoted_ident(&mut self) -> Option<String> {
        let at = self.at;
        self.skip_space();
        if !self.byte(b'"') {
            self.at = at;
            return None;
        }
        let mut word = String::new();
        loop {
            let rest = &self.source[self.at..];
            let end = rest.iter().position(|b| *b == b'"')?;
            word.push_str(&String::from_utf8_lossy(&rest[..end]));
            self.at = self.at.saturating_add(end).saturating_add(1);
            if self.byte(b'"') {
                word.push('"');
            } else {
                break;
            }
        }
        let valid = word.chars().next().is_some_and(store_name_start)
            && word.chars().all(store_name_continue);
        if valid {
            Some(word)
        } else {
            self.at = at;
            None
        }
    }

    /// A literal rendered into the text itself, re-escaped the way the store
    /// spells it — the HAVING path, where a bound parameter would read to the
    /// store's grouped-read check as a name that is neither key nor fold.
    /// Numbers pass as the checked token they parsed from; strings re-quote
    /// with the dialect's one escape; a decimal refuses by name rather than
    /// reading as a float and drifting.
    pub(crate) fn inline(&mut self, value: Value) -> Result<String, Refused> {
        match value {
            Value::Number(tessaridb::Number::Integer(held)) => Ok(held.to_string()),
            Value::Number(tessaridb::Number::Float(held)) => Ok(held.to_string()),
            Value::Number(tessaridb::Number::Decimal(_)) => Err(Refused::surface(
                code::FEATURE,
                "a decimal literal in HAVING is not spoken in this build",
            )),
            Value::String(text) => Ok(format!("'{}'", text.replace('\'', "''"))),
            Value::Bool(held) => Ok(if held {
                "true".to_owned()
            } else {
                "false".to_owned()
            }),
            Value::Null => Ok("NULL".to_owned()),
            Value::None => Ok("NONE".to_owned()),
            other => Err(Refused::with_detail(
                code::FEATURE,
                "this literal is not spoken inline in this build",
                format!("{other:?}"),
            )),
        }
    }

    /// A fold of the dialect: `count(*)`, `count(x)`, `count(DISTINCT x)`,
    /// `sum`, `avg` (the store's `mean`), `mean`, `min`, `max`, `median` over a
    /// field path. `avg` is the one renamed mapping — the engine's fold owns
    /// the name `mean`, and the wire answers under it.
    pub(crate) fn aggregate(&mut self) -> Result<Option<String>, Refused> {
        const FOLDS: [&str; 7] = ["count", "sum", "avg", "mean", "min", "max", "median"];
        let at = self.at;
        self.skip_space();
        let Some(word) = self.bare_ident_raw() else {
            return Ok(None);
        };
        let name = word.to_ascii_lowercase();
        if !FOLDS.contains(&name.as_str()) || !self.open_paren() {
            self.at = at;
            return Ok(None);
        }
        let mapped: &str = if name == "avg" { "mean" } else { name.as_str() };
        let argument = if self.star() {
            if name != "count" {
                return Err(Refused::surface(
                    code::FEATURE,
                    "only count takes * in this build",
                ));
            }
            "*".to_owned()
        } else if self.keyword("DISTINCT").is_ok() {
            format!("DISTINCT {}", self.ident_path()?)
        } else {
            self.ident_path()?
        };
        self.close_paren()?;
        Ok(Some(format!("{mapped}({argument})")))
    }

    /// A bare word at the cursor, folded: what SQL spells without quotes.
    pub(crate) fn bare_ident(&mut self) -> Option<String> {
        self.skip_space();
        let word = self.bare_ident_raw()?;
        Some(word.to_ascii_lowercase())
    }

    /// A whole number for LIMIT.
    pub(crate) fn number(&mut self) -> Result<u64, Refused> {
        let word = self.digits();
        if word.is_empty() {
            return Err(Refused::surface(code::SYNTAX, "a number was expected"));
        }
        word.parse::<u64>()
            .map_err(|_| Refused::surface(code::SYNTAX, "the number does not fit"))
    }

    // The lexical atoms, each leaving the cursor past what it ate.

    pub(crate) fn keyword(&mut self, word: &str) -> Result<(), ()> {
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

    pub(crate) fn operator(&mut self, mark: &str) -> bool {
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

    pub(crate) fn comma(&mut self) -> Result<(), ()> {
        self.eat(b',')
    }

    pub(crate) fn open_paren(&mut self) -> bool {
        self.eat(b'(').is_ok()
    }

    pub(crate) fn close_paren(&mut self) -> Result<(), Refused> {
        self.eat(b')')
            .map_err(|_| Refused::surface(code::SYNTAX, "a closing parenthesis is missing"))
    }

    pub(crate) fn star(&mut self) -> bool {
        self.eat(b'*').is_ok()
    }

    fn quote(&mut self) -> bool {
        self.eat(b'\'').is_ok()
    }

    pub(crate) fn dot(&mut self) -> bool {
        self.eat(b'.').is_ok()
    }

    pub(crate) fn byte(&mut self, want: u8) -> bool {
        if self.source.get(self.at) == Some(&want) {
            self.at = self.at.saturating_add(1);
            true
        } else {
            false
        }
    }

    pub(crate) fn eat(&mut self, want: u8) -> Result<(), ()> {
        let at = self.at;
        self.skip_space();
        if self.byte(want) {
            Ok(())
        } else {
            self.at = at;
            Err(())
        }
    }

    pub(crate) fn digits(&mut self) -> String {
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

    /// Digits at the cursor, contiguous; the caller owns spacing.
    pub(crate) fn digits_raw(&mut self) -> String {
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
    pub(crate) fn bare_ident_raw(&mut self) -> Option<String> {
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

    pub(crate) fn skip_space(&mut self) {
        while let Some(byte) = self.source.get(self.at) {
            if !byte.is_ascii_whitespace() {
                break;
            }
            self.at = self.at.saturating_add(1);
        }
    }

    pub(crate) fn slice(&self, start: usize) -> &str {
        std::str::from_utf8(&self.source[start..self.at]).unwrap_or("")
    }

    /// Whether a double quote sits at the cursor, without consuming it.
    pub(crate) fn peek_quoted(&mut self) -> bool {
        let at = self.at;
        self.skip_space();
        let quoted = self.source.get(self.at) == Some(&b'"');
        self.at = at;
        quoted
    }

    /// Whether an opening parenthesis sits at the cursor, without consuming
    /// it — the mark of a subquery source, which this build refuses by name.
    pub(crate) fn peek_paren(&mut self) -> bool {
        let at = self.at;
        self.skip_space();
        let paren = self.source.get(self.at) == Some(&b'(');
        self.at = at;
        paren
    }
}
