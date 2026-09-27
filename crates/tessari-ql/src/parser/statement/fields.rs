//! Field declarations and the kinds a field may hold.

use super::Parser;
use tessari_types::{FieldKind, Number};

use crate::ast::{Approximation, ColumnDeclaration, Name, StatementKind, TableRef};
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};

use super::FieldOptions;

impl Parser<'_> {
    /// The name of a field being declared, altered or dropped.
    ///
    /// A field name in an object literal already reads with `field_name()`,
    /// which takes a quoted name — so `{ 'password': '…' }` writes a field that
    /// `DEFINE FIELD password …` could not declare, because `PASSWORD` is a
    /// keyword and `name()` does not accept one. The write worked and the
    /// declaration did not, and quoting did not help either: the two halves of
    /// the language disagreed about what a field may be called.
    ///
    /// This accepts the quoted form here too, and **only** the quoted form. A
    /// bare keyword is still refused, which keeps the wider question — whether
    /// `DEFINE FIELD password` should read as a name — open and separate
    /// (Q-417). The narrow version is worth having on its own because a string
    /// literal in a name position cannot be anything else: no existing statement
    /// changes meaning, only refusals become parses, and a statement that forgot
    /// its name is still missing a name, because a missing name is not a string.
    ///
    /// It matters most in a vault, which is strict, so a field nobody can
    /// declare is a field a vault cannot hold — and `password` is the first
    /// thing a secret store will be asked for.
    pub(super) fn declared_field_name(&mut self) -> Result<Name> {
        if matches!(self.peek(), Some(Token::Str(_))) {
            let (text, span) = self.quoted_field_name()?;
            return Ok(Name { text, span });
        }
        self.name()
    }

    /// `DEFINE FIELD email ON users TYPE string`
    pub(super) fn define_field(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.declared_field_name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the field is on")?;
        let table = self.table_ref()?;
        self.declaration_tail(name, table, if_not_exists, false)
    }

    /// The half of a field declaration that follows its name and its table.
    ///
    /// Shared with `ALTER TABLE … ADD FIELD` and `… ALTER FIELD`, which name the
    /// same two things in the other order and then say exactly the same thing
    /// about the field. Written once so the two spellings cannot drift — a
    /// second copy is how `DEFAULT` ends up accepted by one of them and not the
    /// other.
    pub(super) fn field_declaration(
        &mut self,
        name: Name,
        table: TableRef,
        replacing: bool,
    ) -> Result<StatementKind> {
        self.declaration_tail(name, table, false, replacing)
    }

    pub(super) fn declaration_tail(
        &mut self,
        name: Name,
        table: TableRef,
        if_not_exists: bool,
        replacing: bool,
    ) -> Result<StatementKind> {
        self.expect_keyword(Keyword::Type, "`TYPE` and what the field may hold")?;
        let kind = self.field_kind()?;
        let marker = self.span_here();
        let FieldOptions {
            required,
            secret,
            default,
            analyzer,
            assert,
        } = self.field_options()?;
        if replacing {
            if secret {
                // There is no `ALTER FIELD … SECRET`. Turning the marker on
                // leaves every record already written in the clear; turning it
                // off leaves every record already written unreadable. Both are a
                // field that is half sealed, and a statement that produced
                // either would report success.
                return Err(Error::Unsupported {
                    feature: "altering a field to or from `SECRET` — declare it \
                              `SECRET` when the vault's field is defined",
                    span: marker,
                });
            }
            return Ok(StatementKind::AlterField {
                name,
                table,
                kind,
                required,
                default,
                analyzer,
                assert,
            });
        }
        Ok(StatementKind::DefineField {
            name,
            table,
            kind,
            required,
            secret,
            default,
            analyzer,
            assert,
            if_not_exists,
        })
    }

    /// Everything a field declaration says after its type.
    ///
    /// Shared by all three spellings — `DEFINE FIELD`, `ALTER TABLE … FIELD`,
    /// and a column inside `DEFINE TABLE`'s parentheses — because a reader who
    /// learns `DEFAULT` in one of them has learned it in the others, and three
    /// copies of this loop is how one of them quietly stops accepting `ASSERT`.
    pub(super) fn field_options(&mut self) -> Result<FieldOptions> {
        // Any marker, in any order, and none twice — the rule `DEFINE TABLE`'s
        // two flags already follow, for the same reason: there is no reading
        // under which one has to precede the other, and a grammar that insisted
        // would only be remembered wrong.
        let mut options = FieldOptions::default();
        loop {
            if !options.required && self.eat_keyword(Keyword::Required) {
                options.required = true;
            } else if options.default.is_none() && self.eat_keyword(Keyword::Default) {
                options.default = Some(self.written_expression()?);
            } else if !options.secret && self.eat_word("secret") {
                // Contextual, like `assert` and `vector` above. `secret` is an
                // ordinary column name in plenty of schemas and reserving it
                // here would reserve it in every position, including as the name
                // of the very field somebody is trying to declare.
                options.secret = true;
            } else if options.analyzer.is_none() && self.eat_keyword(Keyword::Analyzer) {
                options.analyzer = Some(self.name()?);
            } else if options.assert.is_none() && self.eat_word("assert") {
                // Contextual, like `vector` and `fetch`: nothing but this marker
                // can stand here, and a field called `assert` is not a name to
                // take away from a table that has one.
                options.assert = Some(crate::parser::assertion::lower(&self.condition()?)?);
            } else {
                break;
            }
        }
        Ok(options)
    }

    /// The parenthesised column list of a columnar `DEFINE TABLE`, if it has one.
    ///
    /// Empty parentheses are refused rather than read as no columns: the
    /// flag-only spelling already says *no columns* by writing nothing, so `()`
    /// can only be a list somebody meant to fill in.
    pub(super) fn columns(&mut self) -> Result<Vec<ColumnDeclaration>> {
        if !self.eat_punct(Punct::ParenOpen) {
            return Ok(Vec::new());
        }
        let mut columns = Vec::new();
        loop {
            let name = self.name()?;
            let kind = self.field_kind()?;
            let marker = self.span_here();
            let FieldOptions {
                required,
                secret,
                default,
                analyzer,
                assert,
            } = self.field_options()?;
            if secret {
                // A columnar `DEFINE TABLE` is not a vault and cannot become
                // one, so a `SECRET` here has nowhere to be honoured. Refused
                // rather than parsed and dropped: a marker silently ignored is
                // the failure this whole feature exists to prevent.
                return Err(Error::Unsupported {
                    feature: "`SECRET` in a columnar table declaration — a \
                              secret field belongs on a `DEFINE VAULT`",
                    span: marker,
                });
            }
            columns.push(ColumnDeclaration {
                name,
                kind,
                required,
                default,
                analyzer,
                assert,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::ParenClose, "`)` closing the column list")?;
        Ok(columns)
    }

    /// A type name, which may be spelled with a reserved word.
    ///
    /// `table`, `set`, `range`, `datetime` and `uuid` are all reserved
    /// elsewhere, and after `TYPE` nothing but a type name can appear — so the
    /// word is read as text here, the way a field name inside an object literal
    /// already is. The alternative is a language where five of the seventeen
    /// types cannot be written down.
    pub(super) fn field_kind(&mut self) -> Result<FieldKind> {
        // A union is spelled by its members, so it is recognised by one of them
        // standing where a type name would. Nothing else in a declaration puts
        // a string here, so the two readings cannot collide.
        if matches!(self.peek(), Some(Token::Str(_))) {
            return self.literal_union();
        }
        // Read from the tokens rather than through `FieldKind::parse`, which
        // takes a single spelling: a width is four tokens. Contextual like
        // `order` and `fetch`, and for the reason `DEFINE INDEX … VECTOR`
        // already gives — a field called `vector` in a database of embeddings is
        // not a name to take away. It costs nothing here, because the name is
        // read before the type in both declarations that reach this.
        if self.eat_word("vector") {
            return self.vector_width();
        }
        let spelling = match self.peek() {
            Some(Token::Keyword(keyword)) => keyword.spelling().to_owned(),
            Some(Token::Ident(name)) => name.clone(),
            _ => return Err(self.error_here("a type name")),
        };
        let Some(kind) = FieldKind::parse(&spelling) else {
            return Err(self.error_here("a type name"));
        };
        self.advance();
        Ok(kind)
    }

    /// `<768>` — how many numbers every vector in this field holds.
    ///
    /// The width is required, and there is no width-less `vector`. A vector
    /// whose length is not declared is an `array`, which the language already
    /// has: the word would say something about the author's intention and
    /// nothing that could be checked, and a field that looks checked and is not
    /// is worse than one that never claimed to be.
    ///
    /// A **literal**, like `DEPTH n` and for a related reason. A width read from
    /// a parameter would be a schema whose shape depends on what was bound at
    /// the moment the declaration ran, and the catalog has to store one answer.
    /// `APPROXIMATE`, and the budget it may carry.
    ///
    /// `EFFORT` stands only after `APPROXIMATE` — never on its own and never
    /// before it — because a budget without the permission is a number with
    /// nothing to spend it on: an exact scan visits every record by definition.
    /// So the pair is read here as one thing and stored as one value, and the
    /// illegal half is not expressible.
    ///
    /// Both words are contextual, like the rest of this tail: a field called
    /// `approximate` or `effort` stays a field.
    pub(super) fn approximation(&mut self) -> Result<Option<Approximation>> {
        if !self.eat_word("approximate") {
            return Ok(None);
        }
        if !self.eat_word("effort") {
            return Ok(Some(Approximation::Default));
        }
        let span = self.span_here();
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here("a whole number of candidates, written out"));
        };
        // A negative and a zero refuse as the same thing, as they do for a width
        // and for a depth: both say fewer than one candidate, and a walk that may
        // keep none is a search with no way to answer.
        let candidates = usize::try_from(*written).unwrap_or(0);
        self.advance();
        if candidates == 0 {
            return Err(Error::EffortBelowOne { span });
        }
        Ok(Some(Approximation::Effort(candidates)))
    }

    /// `WITHOUT SCAN GUARD`, which lifts the planner's veto for this read.
    ///
    /// Three words rather than one, and that is the point. The clause changes
    /// which plan runs, so a reader skimming the tail must not be able to take
    /// it for decoration — `USING INDEX` was rejected as the place to put it for
    /// the same reason, since a modifier that turns an assertion into an
    /// instruction is a pun.
    ///
    /// All three words are contextual, like the rest of this tail: a field, a
    /// table or an index called `without`, `scan` or `guard` stays itself.
    /// `WITHOUT` only begins this clause where a clause may begin, and once it
    /// has, the two words after it are required — a bare `WITHOUT` names nothing
    /// this planner has, and guessing at what was meant would be inventing a
    /// second spelling nobody documented.
    pub(super) fn scan_guard(&mut self) -> Result<bool> {
        if !self.eat_word("without") {
            return Ok(false);
        }
        self.expect_word("scan", "`SCAN GUARD` — the guard `WITHOUT` lifts")?;
        self.expect_word("guard", "`GUARD`, completing `WITHOUT SCAN GUARD`")?;
        Ok(true)
    }

    pub(super) fn vector_width(&mut self) -> Result<FieldKind> {
        self.expect_punct(Punct::Less, "`<` and the width every vector here holds")?;
        let span = self.span_here();
        let width = self.vector_dimension()?;
        self.expect_punct(Punct::Greater, "`>` closing the width")?;
        FieldKind::vector(width).ok_or(Error::VectorWidthBelowOne { span })
    }

    /// The whole number of components a declaration names.
    ///
    /// Shared by the two places a width is written — `TYPE vector<n>` on a field
    /// and `DIMENSION n` on a store — so that the language has one answer to
    /// *which numbers are widths* rather than one answer per doorway. That is
    /// the same reason `field_kind` is called by both field spellings.
    pub(super) fn vector_dimension(&mut self) -> Result<usize> {
        let span = self.span_here();
        let Some(Token::Number(Number::Integer(written))) = self.peek() else {
            return Err(self.error_here("a whole number of components, written out"));
        };
        // A negative and a zero refuse as the same thing, which they are: both
        // say fewer than one component, and a declaration that can hold only the
        // empty array is one no useful write satisfies.
        let written = usize::try_from(*written).unwrap_or(0);
        self.advance();
        if written > crate::WIDEST_VECTOR {
            return Err(Error::VectorWidthAboveTheCeiling {
                most: crate::WIDEST_VECTOR,
                span,
            });
        }
        // Asked of the type rather than tested here, so the rule lives where the
        // kind that carries it lives and cannot drift from it.
        match FieldKind::vector(written) {
            Some(FieldKind::Vector(width)) => Ok(width),
            _ => Err(Error::VectorWidthBelowOne { span }),
        }
    }

    /// `'draft' | 'published'` — a field that holds one of a fixed set of strings.
    ///
    /// The declaration a status column has always wanted. `TYPE string` is true
    /// and says nothing; an `ASSERT` says the same thing but says it where a
    /// reader of the schema does not look, and where a reader of an error
    /// message gets a condition rather than a list.
    ///
    /// Members are sorted and deduplicated by the constructor, so two
    /// declarations naming the same set are the same type however they were
    /// typed. A set that remembers the order somebody wrote it in is two values
    /// for one fact — the rule a grant's verbs already follow.
    pub(super) fn literal_union(&mut self) -> Result<FieldKind> {
        let mut members = Vec::new();
        loop {
            let Some(Token::Str(member)) = self.peek() else {
                return Err(self.error_here("a quoted member of the union"));
            };
            members.push(member.clone());
            self.advance();
            if !self.eat_punct(Punct::Pipe) {
                break;
            }
        }
        // Unreachable while the loop pushes before it can break, and named
        // rather than unwrapped because `union` refusing an empty set is a rule
        // about the type and not about this parser.
        FieldKind::union(members).ok_or_else(|| self.error_here("a member of the union"))
    }
}
