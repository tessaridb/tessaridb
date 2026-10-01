//! `DEFINE SEARCH`, `DEFINE SYNONYMS`, `DEFINE STOPWORDS` and `FROM SEARCH`
//! (ADR-0105).

use crate::ast::{
    Expr, ExprKind, SearchAsk, SearchField, SearchMember, SearchOperator, Source, StatementKind,
};
use crate::error::Result;
use crate::parser::Parser;
use crate::token::{Keyword, Punct, Token};

impl Parser<'_> {
    /// `DEFINE SEARCH [IF NOT EXISTS] <name> ON <table> FIELDS <field> [options],
    /// … [ON …] ANALYZER <name> [STOPWORDS <name>]`
    ///
    /// A field's options are words after it and before the comma, each said
    /// once: `WEIGHT w`, `NO FUZZY`, `NO PREFIX`, `NO PHRASE`, `SYNONYMS <set>`,
    /// `SNIPPET`. Whether a table appears twice or a weight is positive is the
    /// store's question, answered where the span is still in hand.
    pub(crate) fn define_search(&mut self) -> Result<StatementKind> {
        self.advance();
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let mut members = Vec::new();
        while self.eat_keyword(Keyword::On) {
            let table = self.table_ref()?;
            self.expect_keyword(
                Keyword::Fields,
                "`FIELDS` and the fields the table searches",
            )?;
            let mut fields = vec![self.search_field()?];
            while self.eat_punct(Punct::Comma) {
                fields.push(self.search_field()?);
            }
            members.push(SearchMember { table, fields });
        }
        if members.is_empty() {
            return Err(self.error_here("`ON` and a table the search reads"));
        }
        self.expect_keyword(
            Keyword::Analyzer,
            "`ANALYZER` and the analyzer the search reads with",
        )?;
        let analyzer = self.name()?;
        let stopwords = if self.eat_word("stopwords") {
            Some(self.name()?)
        } else {
            None
        };
        Ok(StatementKind::DefineSearch {
            name,
            members,
            analyzer,
            stopwords,
            if_not_exists,
        })
    }

    /// One field of a search and the options written after it.
    fn search_field(&mut self) -> Result<SearchField> {
        let path = self.field_path()?;
        let mut field = SearchField {
            path,
            weight: None,
            fuzzy: true,
            prefix: true,
            phrase: true,
            synonyms: None,
            snippet: false,
        };
        let mut said: Vec<&'static str> = Vec::new();
        loop {
            let option = if self.eat_word("weight") {
                let Some(Token::Number(weight)) = self.peek() else {
                    return Err(self.error_here("a weight"));
                };
                field.weight = Some(weight.clone());
                self.advance();
                "WEIGHT"
            } else if self.eat_word("no") {
                if self.eat_keyword(Keyword::Fuzzy) {
                    field.fuzzy = false;
                    "NO FUZZY"
                } else if self.eat_keyword(Keyword::Prefix) {
                    field.prefix = false;
                    "NO PREFIX"
                } else if self.eat_word("phrase") {
                    field.phrase = false;
                    "NO PHRASE"
                } else {
                    return Err(self.error_here("`FUZZY`, `PREFIX` or `PHRASE` after `NO`"));
                }
            } else if self.eat_word("synonyms") {
                field.synonyms = Some(self.name()?);
                "SYNONYMS"
            } else if self.eat_word("snippet") {
                field.snippet = true;
                "SNIPPET"
            } else {
                return Ok(field);
            };
            if said.contains(&option) {
                return Err(self.error_here("each field option said once"));
            }
            said.push(option);
        }
    }

    /// `DEFINE SYNONYMS [IF NOT EXISTS] <name> { word: ['alternative', …], … }`
    pub(crate) fn define_synonyms(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        self.expect_punct(Punct::BraceOpen, "`{` and the set's words")?;
        let mut entries = Vec::new();
        if !self.eat_punct(Punct::BraceClose) {
            loop {
                let word = match self.peek() {
                    Some(Token::Ident(word) | Token::Str(word)) => word.clone(),
                    _ => return Err(self.error_here("a word")),
                };
                self.advance();
                self.expect_punct(Punct::Colon, "`:` and the word's alternatives")?;
                entries.push((word, self.word_list()?));
                if self.eat_punct(Punct::BraceClose) {
                    break;
                }
                self.expect_punct(Punct::Comma, "`,` or `}`")?;
            }
        }
        Ok(StatementKind::DefineSynonyms {
            name,
            entries,
            if_not_exists,
        })
    }

    /// `DEFINE STOPWORDS [IF NOT EXISTS] <name> ['the', 'a', …]`
    pub(crate) fn define_stopwords(&mut self) -> Result<StatementKind> {
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.name()?;
        let words = self.word_list()?;
        Ok(StatementKind::DefineStopwords {
            name,
            words,
            if_not_exists,
        })
    }

    /// `['a', 'b']`: a bracketed list of string literals.
    fn word_list(&mut self) -> Result<Vec<String>> {
        self.expect_punct(Punct::BracketOpen, "`[` and a list of words")?;
        let mut words = Vec::new();
        if self.eat_punct(Punct::BracketClose) {
            return Ok(words);
        }
        loop {
            let Some(Token::Str(word)) = self.peek() else {
                return Err(self.error_here("a word, in quotes"));
            };
            words.push(word.clone());
            self.advance();
            if self.eat_punct(Punct::BracketClose) {
                return Ok(words);
            }
            self.expect_punct(Punct::Comma, "`,` or `]`")?;
        }
    }

    /// The rest of `FROM SEARCH <name> MATCHES … | COMPLETE …`, after `SEARCH`.
    pub(in crate::parser) fn search_source(&mut self) -> Result<Source> {
        let name = self.name()?;
        let ask = if self.eat_keyword(Keyword::Matches) {
            let operator = if self.eat_keyword(Keyword::Prefix) {
                SearchOperator::Prefix
            } else if self.eat_keyword(Keyword::Fuzzy) {
                SearchOperator::Fuzzy
            } else if self.eat_word("infix") {
                SearchOperator::Infix
            } else {
                SearchOperator::Words
            };
            SearchAsk::Matches {
                operator,
                query: Box::new(self.search_text()?),
            }
        } else if self.eat_word("complete") {
            SearchAsk::Complete {
                beginning: Box::new(self.search_text()?),
            }
        } else {
            return Err(self.error_here("`MATCHES` or `COMPLETE` after the search's name"));
        };
        let condition = if self.eat_keyword(Keyword::Where) {
            Some(Box::new(self.condition()?))
        } else {
            None
        };
        Ok(Source::Search {
            name,
            ask,
            condition,
        })
    }

    /// The text asked of a search: a string literal or a parameter.
    fn search_text(&mut self) -> Result<Expr> {
        let span = self.span_here();
        let kind = match self.peek() {
            Some(Token::Str(text)) => ExprKind::Literal(tessari_types::Value::from(text.as_str())),
            Some(Token::Parameter(name)) => ExprKind::Parameter(name.clone()),
            _ => return Err(self.error_here("the query, as text or a parameter")),
        };
        self.advance();
        Ok(Expr { kind, span })
    }
}
