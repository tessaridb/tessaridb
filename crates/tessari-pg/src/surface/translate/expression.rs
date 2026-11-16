//! The expression layer: projections and the boolean conditions the clauses
//! share — `or` of `and`s of `not`s of predicates, with the operand shapes
//! each side of a comparison may take.

use super::{Columns, Item, Projection, UNNAMED, is_keyword};
use super::{Refused, code};

use super::parser::Parser;

impl Parser<'_> {
    /// `*` or a comma list of fields, folds, placeholders and literals, each
    /// with its alias.
    pub(crate) fn projection(&mut self) -> Result<Projection, Refused> {
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

    /// One projection item: a placeholder, an aggregate, a field path or a
    /// literal, each with its optional alias.
    fn item(&mut self) -> Result<Item, Refused> {
        if let Some(held) = self.placeholder() {
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{held} AS {alias}"), alias),
                None => (held, UNNAMED.to_owned()),
            };
            return Ok(Item { text, column });
        }
        if let Some(call) = self.aggregate()? {
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{call} AS {alias}"), alias),
                None => {
                    let name = call.split('(').next().unwrap_or(&call).to_owned();
                    (call, name)
                }
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
    pub(crate) fn bare_alias(&mut self) -> Option<String> {
        let at = self.at;
        let word = self.bare_ident()?;
        if is_keyword(&word) {
            self.at = at;
            return None;
        }
        Some(word)
    }

    /// A boolean expression: `or` of `and`s of `not`s.
    pub(crate) fn or(&mut self) -> Result<String, Refused> {
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

    /// A comparison, an `IS [NOT] NULL`, an `IN (SELECT …)`, or a
    /// parenthesized expression.
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
        if self.keyword("IN").is_ok() {
            if !self.open_paren() {
                return Err(Refused::surface(
                    code::SYNTAX,
                    "IN wants a parenthesised subquery",
                ));
            }
            if self.keyword("SELECT").is_err() {
                return Err(Refused::surface(
                    code::FEATURE,
                    "IN takes a subquery in this build; a value list is not spoken",
                ));
            }
            let inner = self.shape()?;
            self.close_paren()?;
            let nested = inner.script.strip_suffix(';').unwrap_or(&inner.script);
            return Ok(format!("{left} IN ({nested})"));
        }
        Err(Refused::with_detail(
            code::SYNTAX,
            "a condition wants a comparison",
            format!("after {left:?}"),
        ))
    }

    /// One side of a comparison: a placeholder or an aggregate passes as its
    /// own text; a literal binds (or renders inline inside HAVING); a field
    /// path passes.
    fn operand(&mut self) -> Result<String, Refused> {
        if let Some(held) = self.placeholder() {
            return Ok(held);
        }
        if let Some(call) = self.aggregate()? {
            return Ok(call);
        }
        if let Some(literal) = self.literal()? {
            return Ok(if self.inline_literals {
                self.inline(literal)?
            } else {
                self.bind(literal)
            });
        }
        self.ident_path()
    }

    /// A dotted field path, each part checked as a name.
    pub(crate) fn ident_path(&mut self) -> Result<String, Refused> {
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
    pub(crate) fn ident(&mut self) -> Result<String, Refused> {
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
}
