//! The expression layer: projections and the boolean conditions the clauses
//! share — `or` of `and`s of `not`s of predicates, with the operand shapes
//! each side of a comparison may take and the arithmetic the dialect's
//! clients ask of a projection.

use super::{Columns, Computed, Item, Projection, UNNAMED, is_keyword};
use super::{Refused, code};

use super::parser::Parser;

/// One parsed arithmetic term: its store-script text, the bare path it is
/// when it is exactly one (what an unaliased item answers under), and
/// whether any operator ran (the store wants a name on those).
#[derive(Debug)]
struct Term {
    text: String,
    path: Option<String>,
    arith: bool,
}

impl Term {
    fn plain(text: String, path: Option<String>) -> Self {
        Self {
            text,
            path,
            arith: false,
        }
    }

    fn operated(text: String) -> Self {
        Self {
            text,
            path: None,
            arith: true,
        }
    }
}

impl Parser<'_> {
    /// `*` or a comma list of fields, folds, arithmetic, placeholders,
    /// literals and CASE mappings, each with its alias.
    pub(crate) fn projection(&mut self) -> Result<Projection, Refused> {
        if self.star() {
            return Ok(Projection {
                text: "*".to_owned(),
                columns: Columns::Star,
                items: Vec::new(),
            });
        }
        let mut text = String::new();
        let mut columns = Vec::new();
        let mut items = Vec::new();
        loop {
            let item = self.item()?;
            text.push_str(&item.text);
            columns.push(item.column.clone());
            items.push(item);
            if self.comma().is_err() {
                break;
            }
            text.push_str(", ");
        }
        Ok(Projection {
            text,
            columns: Columns::Named(columns),
            items,
        })
    }

    /// One projection item: a CASE mapping, a catalog function call, a
    /// placeholder, an aggregate, an arithmetic expression, a field path or a
    /// literal, each with its optional alias.
    fn item(&mut self) -> Result<Item, Refused> {
        if self.keyword("CASE").is_ok() {
            return self.case_item();
        }
        let at = self.at;
        if let Some(call) = self.owner_call()? {
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{call} AS {alias}"), alias),
                None => (call, UNNAMED.to_owned()),
            };
            return Ok(Item {
                text,
                column,
                computed: Some(Computed::OwnerOf),
            });
        }
        self.at = at;
        if let Some(held) = self.placeholder() {
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{held} AS {alias}"), alias),
                None => (held, UNNAMED.to_owned()),
            };
            return Ok(Item {
                text,
                column,
                computed: None,
            });
        }
        if let Some(call) = self.aggregate()? {
            let (text, column) = match self.alias()? {
                Some(alias) => (format!("{call} AS {alias}"), alias),
                None => {
                    let name = call.split('(').next().unwrap_or(&call).to_owned();
                    (call, name)
                }
            };
            return Ok(Item {
                text,
                column,
                computed: None,
            });
        }
        let term = self.sum()?;
        let (text, column) = match self.alias()? {
            Some(alias) => (format!("{} AS {alias}", term.text), alias),
            None => match term.path {
                Some(path) => {
                    let name = path.rsplit('.').next().unwrap_or(&path).to_owned();
                    (term.text, name)
                }
                None if term.arith => {
                    // The store asks a name of a projection that ends in a
                    // position; the dialect shows the same column as
                    // ?column?, so the script alias is the port's own.
                    let alias = self.expr_alias();
                    (format!("{} AS {alias}", term.text), UNNAMED.to_owned())
                }
                None => (term.text, UNNAMED.to_owned()),
            },
        };
        Ok(Item {
            text,
            column,
            computed: None,
        })
    }

    /// `pg_catalog.pg_get_userbyid(<path>)` at the cursor, verbatim: the
    /// catalog answers it from the connection's identity; over the store it
    /// refuses with the store's own words, since no such function lives
    /// there. `None` puts the cursor back.
    fn owner_call(&mut self) -> Result<Option<String>, Refused> {
        let at = self.at;
        let Ok(path) = self.ident_path() else {
            self.at = at;
            return Ok(None);
        };
        if !path.ends_with("pg_get_userbyid") || !self.open_paren() {
            self.at = at;
            return Ok(None);
        }
        let arg = self.ident_path()?;
        self.close_paren()?;
        Ok(Some(format!("{path}({arg})")))
    }

    /// `CASE <path> WHEN <literal> THEN <literal> … [ELSE <literal>] END`:
    /// every arm a literal, the shape the dialect's clients map a kind
    /// through. Over the store this is refused by name; the catalog answers
    /// it from its own rows.
    fn case_item(&mut self) -> Result<Item, Refused> {
        let subject = self.ident_path()?;
        let mut arms = Vec::new();
        loop {
            if self.keyword("WHEN").is_err() {
                break;
            }
            let when = self
                .literal()?
                .ok_or_else(|| Refused::surface(code::SYNTAX, "CASE WHEN wants a literal"))?;
            if self.keyword("THEN").is_err() {
                return Err(Refused::surface(code::SYNTAX, "CASE WHEN wants THEN"));
            }
            let then = self
                .literal()?
                .ok_or_else(|| Refused::surface(code::SYNTAX, "CASE THEN wants a literal"))?;
            arms.push((when, then));
        }
        if arms.is_empty() {
            return Err(Refused::surface(
                code::SYNTAX,
                "CASE wants at least one WHEN",
            ));
        }
        let otherwise = if self.keyword("ELSE").is_ok() {
            Some(
                self.literal()?
                    .ok_or_else(|| Refused::surface(code::SYNTAX, "CASE ELSE wants a literal"))?,
            )
        } else {
            None
        };
        if self.keyword("END").is_err() {
            return Err(Refused::surface(code::SYNTAX, "CASE wants END"));
        }
        let mut text = format!("CASE {subject}");
        for (when, then) in &arms {
            text.push_str(&format!(
                " WHEN {} THEN {}",
                self.inline(when.clone())?,
                self.inline(then.clone())?
            ));
        }
        if let Some(other) = &otherwise {
            text.push_str(&format!(" ELSE {}", self.inline(other.clone())?));
        }
        text.push_str(" END");
        let (text, column) = match self.alias()? {
            Some(alias) => (format!("{text} AS {alias}"), alias),
            None => (text, UNNAMED.to_owned()),
        };
        Ok(Item {
            text,
            column,
            computed: Some(Computed::Case {
                subject,
                arms,
                otherwise,
            }),
        })
    }

    fn alias(&mut self) -> Result<Option<String>, Refused> {
        if self.keyword("AS").is_ok() {
            return Ok(Some(self.ident()?));
        }
        Ok(self.bare_alias())
    }

    /// A bare alias: the word after the item, unless it is a keyword of
    /// the shape itself — `SELECT name FROM …` has no alias on `name`, and
    /// `FROM orders WHERE …` has none on the table.
    pub(crate) fn bare_alias(&mut self) -> Option<String> {
        let at = self.at;
        let word = self.ident().ok()?;
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

    /// A comparison, an `IS [NOT] NULL`, an `IN` over a value list or a
    /// subquery, or a parenthesized expression.
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
        let negated_in = self.keyword("NOT").is_ok();
        if self.keyword("IN").is_ok() {
            return self.in_list(&left, negated_in);
        }
        if negated_in {
            return Err(Refused::surface(code::SYNTAX, "NOT wants IN here"));
        }
        for operator in ["!=", "<>", "<=", ">=", "=", "<", ">"] {
            if self.operator(operator) {
                // `column = $n` tells Parse-time inference which table
                // column the parameter's type rides on.
                if operator == "=" {
                    let at = self.at;
                    if let Some(held) = self.placeholder()
                        && let Some(slot) = Parser::slot_of(&held)
                        && !self
                            .inference
                            .iter()
                            .any(|(held_slot, _)| *held_slot == slot)
                    {
                        self.inference.push((slot, left.clone()));
                        return Ok(format!("{left} = {held}"));
                    }
                    self.at = at;
                }
                let rendered = if operator == "<>" { "!=" } else { operator };
                let right = self.operand()?;
                return Ok(format!("{left} {rendered} {right}"));
            }
        }
        Err(Refused::with_detail(
            code::SYNTAX,
            "a condition wants a comparison",
            format!("after {left:?}"),
        ))
    }

    /// `IN (v1, v2, …)` over a value list: exact as the OR-chain of
    /// equalities, which the defer list's approximation concern does not
    /// survive — a NULL in the list yields the same NULL on both shapes,
    /// under the dialect's own three-valued logic (D2). `IN (SELECT …)`
    /// stays the engine's subquery form.
    fn in_list(&mut self, left: &str, negated: bool) -> Result<String, Refused> {
        if !self.open_paren() {
            return Err(Refused::surface(
                code::SYNTAX,
                "IN wants a parenthesised value list or subquery",
            ));
        }
        if self.keyword("SELECT").is_ok() {
            let inner = self.shape()?;
            self.close_paren()?;
            let nested = inner.script.strip_suffix(';').unwrap_or(&inner.script);
            return Ok(format!("{left} IN ({nested})"));
        }
        let at = self.at;
        self.skip_space();
        let empty = self.source.get(self.at) == Some(&b')');
        self.at = at;
        if empty {
            return Err(Refused::surface(
                code::SYNTAX,
                "IN wants at least one value",
            ));
        }
        let mut chain = String::new();
        loop {
            let value = self.in_value()?;
            if !chain.is_empty() {
                chain.push_str(" OR ");
            }
            chain.push_str(&format!("{left} = {value}"));
            if self.comma().is_err() {
                break;
            }
        }
        self.close_paren()?;
        Ok(if negated {
            format!("NOT ({chain})")
        } else {
            format!("({chain})")
        })
    }

    /// One value of an IN list: a literal binds (or renders inline inside
    /// HAVING), a placeholder passes as its own text.
    fn in_value(&mut self) -> Result<String, Refused> {
        if let Some(held) = self.placeholder() {
            return Ok(held);
        }
        if let Some(literal) = self.literal()? {
            return Ok(if self.inline_literals {
                self.inline(literal)?
            } else {
                self.bind(literal)
            });
        }
        Err(Refused::surface(
            code::FEATURE,
            "an IN value wants a literal or a parameter in this build",
        ))
    }

    /// One side of a comparison: arithmetic over placeholders, aggregates,
    /// literals and field paths, parentheses riding through.
    fn operand(&mut self) -> Result<String, Refused> {
        Ok(self.sum()?.text)
    }

    /// Addition and subtraction, the loosest arithmetic binds.
    fn sum(&mut self) -> Result<Term, Refused> {
        let mut term = self.product()?;
        loop {
            let mark = if self.operator("+") {
                "+"
            } else if self.operator("-") {
                "-"
            } else {
                break;
            };
            let right = self.product()?;
            term = Term::operated(format!("{} {mark} {}", term.text, right.text));
        }
        Ok(term)
    }

    /// Multiplication and division, binding tighter than addition.
    fn product(&mut self) -> Result<Term, Refused> {
        let mut term = self.unary()?;
        loop {
            let mark = if self.operator("*") {
                "*"
            } else if self.operator("/") {
                "/"
            } else {
                break;
            };
            let right = self.unary()?;
            term = Term::operated(format!("{} {mark} {}", term.text, right.text));
        }
        Ok(term)
    }

    /// A leading sign or an atom.
    fn unary(&mut self) -> Result<Term, Refused> {
        if self.operator("-") {
            let atom = self.unary()?;
            return Ok(Term::operated(format!("-{}", atom.text)));
        }
        let _ = self.operator("+");
        self.atom()
    }

    /// One arithmetic atom: a parenthesised expression, a placeholder, an
    /// aggregate, a literal or a field path.
    fn atom(&mut self) -> Result<Term, Refused> {
        if self.open_paren() {
            let inner = self.sum()?;
            self.close_paren()?;
            let mut term = Term::plain(format!("({})", inner.text), inner.path);
            term.arith = inner.arith;
            return Ok(term);
        }
        if let Some(held) = self.placeholder() {
            return Ok(Term::plain(held, None));
        }
        if let Some(call) = self.aggregate()? {
            return Ok(Term::plain(call, None));
        }
        if let Some(literal) = self.literal()? {
            let text = if self.inline_literals {
                self.inline(literal)?
            } else {
                self.bind(literal)
            };
            return Ok(Term::plain(text, None));
        }
        let path = self.ident_path()?;
        Ok(Term::plain(path.clone(), Some(path)))
    }

    /// The port's own alias for an unnamed arithmetic projection — a bare
    /// word the store accepts, shown to the client as `?column?`.
    fn expr_alias(&mut self) -> String {
        self.next_expr = self.next_expr.saturating_add(1);
        format!("e{}", self.next_expr)
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

    /// One name: a bare word folded lowercase, or a double-quoted spelling
    /// kept verbatim (the dialect's quoted identifiers). Names are grammar —
    /// they are checked, never bound. The store's lexer speaks bare words
    /// and preserves case, so a quoted name rides verbatim; contents the
    /// store's name grammar cannot hold refuse by name rather than silently
    /// changing the name.
    pub(crate) fn ident(&mut self) -> Result<String, Refused> {
        if self.peek_quoted() {
            return self.quoted_ident().ok_or_else(|| {
                Refused::surface(
                    code::FEATURE,
                    "quoted identifiers are limited to names the store's lexer speaks in this build",
                )
            });
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
