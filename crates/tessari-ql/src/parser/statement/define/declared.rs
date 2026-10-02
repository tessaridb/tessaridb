//! The declared stores: vectors, series, views, indexes and analyzers.

use super::super::Parser;
use crate::ast::StatementKind;
use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Token};
use tessari_types::Filter;

/// What a refusal of a filter name offers instead. Spelled out because a
/// refusal's expectation is static text; the test below holds it to
/// [`Filter::ALL`], so a filter added there cannot be missing here.
const FILTER_NAMES: &str = "a filter: lowercase, ascii or stemmer";

/// What a refusal of a stemmer's language offers instead, held to
/// [`Filter::ALL`] by the same test.
const STEMMER_LANGUAGES: &str = "a stemmer language: english, russian, german, french or spanish";

impl Parser<'_> {
    /// `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine`
    ///
    /// Both clauses are required and neither has a default. The width, because
    /// declaring it is the whole capability the word adds. The distance, for the
    /// reason the index already records: a default would silently decide which
    /// queries the store can serve, and a graph built for one distance
    /// approximates that distance and no other.
    ///
    /// They are read in a fixed order rather than in any order. Two clauses is
    /// too few to be worth an order-free reader, and a fixed order is what makes
    /// the statement read the same way in every store that has one.
    pub(crate) fn define_vector(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("dimension") {
            return Err(self.error_here("`DIMENSION` and how wide every vector here is"));
        }
        let dimension = self.vector_dimension()?;
        if !self.eat_word("distance") {
            return Err(self.error_here("`DISTANCE` and the distance its index is built with"));
        }
        Ok(StatementKind::DefineVector {
            name,
            dimension,
            distance: self.name()?,
            // Optional and last: a store is full precision unless it says so,
            // and the word reads after the distance it is a form of.
            quantized: self.eat_word("quantized"),
            if_not_exists,
        })
    }

    /// `DEFINE SERIES readings RETAIN 30d [TIME at]`
    ///
    /// The retention is a literal duration rather than an expression, on
    /// [`Self::define_queue`]'s rule and for its reason: a floor a bound value
    /// could set is a floor a caller could move, and this one is meant to be
    /// readable in the statement that declared it.
    pub(crate) fn define_series(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_word("retain") {
            return Err(self.error_here("`RETAIN` and how far back the table answers"));
        }
        let expected = "a duration, like `30d` or `12h`";
        let Some(Token::Duration(written)) = self.peek() else {
            return Err(self.error_here(expected));
        };
        let retain = *written;
        let at = self.span_here();
        self.advance();
        // Refused here for the reason a queue's zero timeout is: a retention of
        // no length is not a retention, it is a table that answers with nothing,
        // and that is a mistake in the statement rather than a configuration.
        if retain.seconds() < 0 || (retain.seconds() == 0 && retain.nanos() == 0) {
            return Err(Error::EmptyRetention {
                written: retain.to_literal(),
                span: at,
            });
        }
        // `TIME at` names the field the identity is minted from; the field's
        // kind is checked where a record is written, because a series declares
        // no columns for a type to be checked against here.
        let time = if self.eat_word("time") {
            Some(self.name()?)
        } else {
            None
        };
        Ok(StatementKind::DefineSeries {
            name,
            retain,
            time,
            if_not_exists,
        })
    }

    /// `DEFINE ROLLUP hourly FROM readings WINDOW 1h [BY sensor] COMPUTE
    /// count(*) AS n, sum(v) AS total RETAIN 365d`
    ///
    /// Read in a fixed order, as `DEFINE VECTOR`'s clauses are. Which folds a
    /// rollup keeps is the session's to judge, where the refusal can say why.
    pub(crate) fn define_rollup(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_keyword(Keyword::From) {
            return Err(self.error_here("`FROM` and the series it folds"));
        }
        let source = self.name()?;
        if !self.eat_word("window") {
            return Err(self.error_here("`WINDOW` and how wide each window is"));
        }
        let Some(Token::Duration(window)) = self.peek() else {
            return Err(self.error_here("a duration, like `1h`"));
        };
        let window = *window;
        self.advance();
        let by = if self.eat_word("by") {
            Some(self.name()?)
        } else {
            None
        };
        if !self.eat_word("compute") {
            return Err(self.error_here("`COMPUTE` and what each window keeps"));
        }
        let mut computes = Vec::new();
        loop {
            let fold = self.name()?;
            self.expect_punct(Punct::ParenOpen, "`(` after the fold")?;
            let of = if self.eat_punct(Punct::Star) {
                None
            } else {
                Some(self.name()?)
            };
            self.expect_punct(Punct::ParenClose, "`)` after what is folded")?;
            if !self.eat_keyword(Keyword::As) {
                return Err(self.error_here("`AS` and the name the value is kept under"));
            }
            computes.push((fold, of, self.name()?));
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if !self.eat_word("retain") {
            return Err(self.error_here("`RETAIN` and how far back the rollup answers"));
        }
        let Some(Token::Duration(retain)) = self.peek() else {
            return Err(self.error_here("a duration, like `365d`"));
        };
        let retain = *retain;
        self.advance();
        Ok(StatementKind::DefineRollup {
            name,
            source,
            window,
            by,
            computes,
            retain,
            if_not_exists,
        })
    }

    /// `DEFINE VIEW active AS SELECT * FROM users WHERE active = true`
    ///
    /// The read runs to the end of the statement. No parentheses, because there
    /// is nothing to disambiguate: a view holds exactly one `SELECT` and it is
    /// everything after `AS`.
    ///
    /// # Parsed to validate, kept as text to store
    ///
    /// The read is parsed here so that a view which is not one `SELECT` is
    /// refused where somebody wrote it rather than on whatever read first names
    /// it, and then the **source text** is what the statement carries — sliced
    /// by the read's own span. Rendering the parsed tree back would store a
    /// different statement that happens to mean the same thing, and a reader
    /// comparing what they wrote against what `INFO` reports should find them
    /// equal.
    ///
    /// Nothing is resolved: the tables the read names need not exist yet, the
    /// same rule a field's `DEFAULT` follows. The alternative would make the
    /// order of a provisioning script load-bearing.
    pub(crate) fn define_view(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        if !self.eat_keyword(Keyword::As) {
            return Err(self.error_here("`AS` and the read this name means"));
        }
        if self.peek_keyword() != Some(Keyword::Select) {
            return Err(self.error_here("`SELECT` — a view is a read"));
        }
        let read = self.select_statement()?;
        Ok(StatementKind::DefineView {
            name,
            read: self.source[read.span.start..read.span.end].to_owned(),
            if_not_exists,
        })
    }

    /// `DEFINE INDEX by_email ON users FIELDS email, name UNIQUE`
    pub(crate) fn define_index(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_keyword(Keyword::On, "`ON` and the table the index reads")?;
        let table = self.table_ref()?;
        self.expect_keyword(Keyword::Fields, "`FIELDS` and the fields to index")?;

        let mut fields = vec![self.field_path()?];
        while self.eat_punct(Punct::Comma) {
            fields.push(self.field_path()?);
        }
        // **One** marker. `UNIQUE` says how entries collide, `SEARCH` says the
        // entries are terms, `VECTOR` says they are a graph, `SPATIAL` says they
        // are cells — four different index kinds wearing four flags, of
        // which at most one can be true. Accepting two used to be possible and
        // the first one checked simply won, so `UNIQUE SEARCH` was an index
        // whose uniqueness was silently ignored. Adding a third made that
        // inconsistency a thing to answer rather than inherit.
        /// Which of the four an index is.
        enum Marker {
            Unique,
            Search,
            Spatial,
            /// With the distance its graph is built for, which is required —
            /// a default would silently decide which queries the index serves.
            Vector(crate::Name),
        }
        let mut kind: Option<Marker> = None;
        loop {
            let held = if self.eat_keyword(Keyword::Unique) {
                Marker::Unique
            } else if self.eat_keyword(Keyword::Search) {
                Marker::Search
            } else if self.eat_word("spatial") {
                // Contextual for the same reason `vector` is: a field called
                // `spatial` is not a name to take away from a caller.
                Marker::Spatial
            } else if self.eat_word("vector") {
                // Contextual, like `order` and `fetch`: a field called `vector`
                // in a database of embeddings is not a name to take away.
                Marker::Vector(self.name()?)
            } else {
                break;
            };
            if kind.is_some() {
                return Err(self.error_here("one index kind, not two"));
            }
            kind = Some(held);
        }
        // What a search index keeps beside its postings (ADR-0100 D4). Words
        // after `SEARCH` only — on any other kind they would describe storage
        // that kind does not have — each said at most once, so a declaration
        // reads back the way it was meant.
        let mut costs = crate::SearchCosts::default();
        loop {
            let word = |parser: &Self, word: &str| matches!(parser.peek(), Some(Token::Ident(found)) if found.eq_ignore_ascii_case(word));
            let seen = if word(self, "positions") {
                costs.positions
            } else if word(self, "offsets") {
                costs.offsets
            } else if word(self, "no") {
                costs.unscored
            } else {
                break;
            };
            // Judged before the word is taken, so the refusal points at it.
            if !matches!(kind, Some(Marker::Search)) {
                return Err(
                    self.error_here("`POSITIONS`, `OFFSETS` and `NO SCORE` only after `SEARCH`")
                );
            }
            if seen {
                return Err(self.error_here("each of `POSITIONS`, `OFFSETS`, `NO SCORE` once"));
            }
            if self.eat_word("positions") {
                costs.positions = true;
            } else if self.eat_word("offsets") {
                costs.offsets = true;
            } else {
                self.eat_word("no");
                if !self.eat_word("score") {
                    return Err(self.error_here("`SCORE` — the one option `NO` turns off"));
                }
                costs.unscored = true;
            }
        }
        // `QUANTIZED` is a form of the vector a graph keeps, so it reads after
        // `VECTOR <distance>` and nowhere else — on any other kind it would
        // describe storage that kind does not have. Judged before the word is
        // taken, so the refusal points at it.
        let quantized = if matches!(self.peek(), Some(Token::Ident(found)) if found.eq_ignore_ascii_case("quantized"))
        {
            if !matches!(kind, Some(Marker::Vector(_))) {
                return Err(self.error_here("`QUANTIZED` only after `VECTOR` and its distance"));
            }
            self.eat_word("quantized")
        } else {
            false
        };
        // An index over `tags[*]` is a **multikey** index: one entry per element
        // rather than one per record. Three shapes are refused, each naming its
        // own reason — a caller told "unexpected token" would go looking for a
        // typo in a statement that has none.
        let mut several = fields.iter().filter(|field| field.path.is_several());
        if let Some(field) = several.next() {
            match &kind {
                Some(Marker::Unique) => {
                    return Err(Error::SeveralInAUniqueIndex { span: field.span });
                }
                Some(Marker::Search | Marker::Vector(_) | Marker::Spatial) => {
                    return Err(Error::SeveralInAnAnalysedIndex { span: field.span });
                }
                None => {}
            }
            if let Some(second) = several.next() {
                return Err(Error::SeveralRoutesInOneIndex { span: second.span });
            }
        }
        Ok(StatementKind::DefineIndex {
            name,
            table,
            fields,
            unique: matches!(kind, Some(Marker::Unique)),
            search: matches!(kind, Some(Marker::Search)),
            costs,
            spatial: matches!(kind, Some(Marker::Spatial)),
            vector: match kind {
                Some(Marker::Vector(distance)) => Some(distance),
                _ => None,
            },
            quantized,
            if_not_exists,
        })
    }

    /// `DEFINE ANALYZER simple FILTERS lowercase, ascii`
    ///
    /// The tokenizer is not named because there is one: splitting on
    /// non-alphanumeric boundaries is what every filter chain assumes
    /// underneath it, and a knob with one setting is a knob nobody should have
    /// to read about.
    pub(crate) fn define_analyzer(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let mut filters = Vec::new();
        if self.eat_keyword(Keyword::Filters) {
            filters.push(self.filter()?);
            while self.eat_punct(Punct::Comma) {
                filters.push(self.filter()?);
            }
        }
        Ok(StatementKind::DefineAnalyzer {
            name,
            filters,
            if_not_exists,
        })
    }

    /// One named filter.
    ///
    /// A word that names no filter is refused AT that word, saying which
    /// filters and languages exist. It used to be refused at the token after
    /// the call, asking for "a filter name" — so `stemmer(dutch)` was reported
    /// as a fault in the `;` and never named the language (Q-873).
    pub(crate) fn filter(&mut self) -> Result<Filter> {
        let Some(Token::Ident(word)) = self.peek() else {
            return Err(self.error_here(FILTER_NAMES));
        };
        let named_at = self.position;
        let mut word = word.clone();
        self.advance();
        let mut language_at = None;
        // `stemmer(russian)`: a filter's one argument, its language.
        if self.eat_punct(Punct::ParenOpen) {
            let Some(Token::Ident(argument)) = self.peek() else {
                return Err(self.error_here("a language"));
            };
            language_at = Some(self.position);
            word = format!("{word}({argument})");
            self.advance();
            if !self.eat_punct(Punct::ParenClose) {
                return Err(self.error_here("`)`"));
            }
        }
        Filter::parse(&word).ok_or_else(|| match language_at {
            Some(at) if word.to_ascii_lowercase().starts_with("stemmer(") => {
                self.error_at(at, STEMMER_LANGUAGES)
            }
            _ => self.error_at(named_at, FILTER_NAMES),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{FILTER_NAMES, STEMMER_LANGUAGES};
    use tessari_types::Filter;

    #[test]
    fn the_refusals_offer_every_filter_and_every_language() {
        for filter in Filter::ALL {
            let name = filter.name();
            let (offered, word) = match name.split_once('(') {
                Some((_, language)) => (STEMMER_LANGUAGES, language.trim_end_matches(')')),
                None => (FILTER_NAMES, name),
            };
            assert!(offered.contains(word), "{word} is missing from {offered:?}");
        }
        // English is spelled as the bare `stemmer`, so its language is held here.
        assert!(STEMMER_LANGUAGES.contains("english"));
    }
}
