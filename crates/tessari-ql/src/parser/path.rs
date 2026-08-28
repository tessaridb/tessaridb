//! Reading a name, and reading a route into a record.
//!
//! One file because they are the same decision at two depths: a plain field name
//! is a route of one step, and the only thing that separates them is whether a
//! delimiter follows. Keeping them apart would have put the rule for what may
//! stand in a name in two places.

use tessari_types::{Number, Path, Step};

use super::Parser;
use crate::ast::{ExprKind, FieldPath, Name, Projected, Projection};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Spanned, Token};

impl Parser<'_> {
    /// A route to a value inside a record: `email`, `address.city`, `tags[0]`.
    ///
    /// Read only where a *value inside a record* is meant — a filter's left side
    /// and an index's projection. It is never read where a table may stand,
    /// because `.` already qualifies a table by its database and `orders.users`
    /// would otherwise start reading as a path into a table called `orders`.
    /// The parser tests assert that separation rather than the grammar being
    /// trusted to keep it.
    pub(super) fn field_path(&mut self) -> Result<FieldPath> {
        let root = self.name()?;
        let start = root.span;
        let mut steps = Vec::new();
        loop {
            if self.eat_punct(Punct::Dot) {
                steps.push(Step::Field(self.name()?.text));
            } else if self.eat_punct(Punct::BracketOpen) {
                // `[*]` is every element; `[3]` is one of them.
                if self.eat_punct(Punct::Star) {
                    steps.push(Step::Every);
                    self.expect_punct(Punct::BracketClose, "`]` after `*`")?;
                } else {
                    steps.push(Step::Index(self.array_position()?));
                    self.expect_punct(Punct::BracketClose, "`]` after a position")?;
                }
            } else {
                break;
            }
        }
        Ok(FieldPath {
            path: Path::new(root.text, steps),
            span: start.to(self.span_behind()),
        })
    }

    /// A position inside an array: a whole number, never negative.
    ///
    /// Counting from the end would need a sign the storage layer has no way to
    /// resolve without knowing the array's length, which is a decision about
    /// what a path *means* rather than how one is written.
    fn array_position(&mut self) -> Result<u64> {
        let expected = "a position, as a whole number";
        let Some(Token::Number(Number::Integer(at))) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let at = u64::try_from(*at).map_err(|_| self.error_here(expected))?;
        self.advance();
        Ok(at)
    }

    /// What a read answers with: `*`, a list of routes with their names, or a
    /// `*` standing among that list.
    ///
    /// A bare `*` keeps its own variant rather than becoming an empty list with
    /// the star set, because that read copies nothing — the records reach the
    /// answer as they were decoded — and it is the commonest read there is.
    pub(super) fn projection(&mut self) -> Result<Projection> {
        let mut everything = None;
        let mut values: Vec<Projected> = Vec::new();
        loop {
            let at = self.span_here();
            if self.eat_punct(Punct::Star) {
                // A second `*` adds nothing the first did not, so it is a typo
                // rather than a meaning, and it is refused where it is written.
                if everything.is_some() {
                    return Err(Error::DuplicateProjection {
                        name: "*".to_owned(),
                        span: at,
                    });
                }
                everything = Some(at);
            } else {
                let next = self.projected()?;
                // Two projections answering under one name would write one field
                // twice into a name-ordered object and keep whichever came last.
                // Knowable from the statement alone, so it is refused here.
                if let Some(clash) = values.iter().find(|held| held.name.text == next.name.text) {
                    return Err(Error::DuplicateProjection {
                        name: clash.name.text.clone(),
                        span: next.name.span,
                    });
                }
                values.push(next);
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if values.is_empty() && everything.is_some() {
            return Ok(Projection::All);
        }
        Ok(Projection::Values { everything, values })
    }

    /// One projected value, and the name it answers under.
    ///
    /// A projection is a **condition-position expression** — a bare name reads
    /// as a route into the record, the same as in a `WHERE` — so
    /// `price * quantity AS total` and `string::upper(address.city) AS shout`
    /// are as writable as `name`.
    ///
    /// The default name is the **last step** of a route, so `address.city`
    /// answers under `city`. Naming it `address.city` would put a delimiter
    /// inside a field name — and a field name carrying a delimiter is precisely
    /// what a route cannot address, so the answer could not be read back by the
    /// grammar that produced it. Anything that is not a bare route has no name
    /// of its own and needs `AS`, for the same reason a position does: every
    /// invented spelling is a convention learned from a surprise.
    fn projected(&mut self) -> Result<Projected> {
        let value = self.condition()?;
        if self.eat_keyword(Keyword::As) {
            let name = self.name()?;
            return Ok(Projected { value, name });
        }
        // Only a bare route names itself. Anything computed — a fold, an
        // arithmetic, a call — has no name of its own, for the same reason a
        // position has none: every invented spelling is a convention learned
        // from a surprise.
        let ExprKind::Path(path) = &value.kind else {
            return Err(Error::UnnamedProjection { span: value.span });
        };
        let text = match path.path.steps().last() {
            None => path.path.root().to_owned(),
            Some(Step::Field(name)) => name.clone(),
            // A position and `[*]` are both un-nameable, and for the same
            // reason: every invented spelling — `tags_0`, `tags`, `_0` — is a
            // convention the author would have to learn from a surprise.
            Some(Step::Index(_) | Step::Every) => {
                return Err(Error::UnnamedProjection { span: value.span });
            }
        };
        let span = value.span;
        Ok(Projected {
            value,
            name: Name { text, span },
        })
    }

    /// A bare name, which is never a keyword.
    /// A name in a position where only a name can stand, reading a reserved
    /// word as one.
    ///
    /// The language already does this after `TYPE`, for an object literal's
    /// field name, and for a function's group before `::`: where nothing but a
    /// name is grammatical, a reserved word is a name and refusing it would be
    /// pedantry that takes a word away from data.
    ///
    /// A grant's verbs are exactly such a position — `GRANT read, write ON …`
    /// admits nothing else between `GRANT` and `ON` — and `read` became a
    /// reserved word when files gained `READ`. Reading the source slice keeps
    /// the verb spelled the way it was written, which matters because the store
    /// compares it case-sensitively.
    pub(super) fn word_or_name(&mut self) -> Result<Name> {
        if let Some(Token::Keyword(_)) = self.peek() {
            let span = self.span_here();
            self.advance();
            let text = self
                .source
                .get(span.start..span.end)
                .unwrap_or_default()
                .to_owned();
            return Ok(Name { text, span });
        }
        self.name()
    }

    pub(super) fn name(&mut self) -> Result<Name> {
        if !matches!(self.peek(), Some(Token::Ident(_))) {
            return Err(self.error_here("a name"));
        }
        let Some(Spanned {
            token: Token::Ident(text),
            span,
        }) = self.advance()
        else {
            return Err(self.error_here("a name"));
        };
        Ok(Name { text, span })
    }
}
