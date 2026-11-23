//! Satellite types for statement kinds — SearchCosts, SearchMember, SearchField, SearchAsk, SearchOperator.
//!
//! Moved out of `statement.rs`; re-exported there, so every
//! existing path is unchanged.

use super::{Expr, FieldPath, Name, TableRef};
use tessari_types::Number;

/// What a `SEARCH` index keeps beside its postings, as the statement said it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchCosts {
    /// `POSITIONS`: each posting carries the term's token ordinals.
    pub positions: bool,
    /// `OFFSETS`: each posting carries the term's byte ranges.
    pub offsets: bool,
    /// `NO SCORE`: no collection statistics, and a score over it is refused.
    pub unscored: bool,
}

/// One table of a `DEFINE SEARCH` and the fields it contributes (ADR-0105).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMember {
    /// The table.
    pub table: TableRef,
    /// Its fields, in the order written — the order a field's statistics are
    /// stored in.
    pub fields: Vec<SearchField>,
}

/// One field of a search, with what it allows and how much it weighs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchField {
    /// The field, possibly nested.
    pub path: FieldPath,
    /// `WEIGHT w`, when written: the field's BM25F weight. Absent is one.
    pub weight: Option<Number>,
    /// Whether a `MATCHES FUZZY` word may be answered here (`NO FUZZY` clears it).
    pub fuzzy: bool,
    /// Whether a prefix word may be answered here (`NO PREFIX` clears it).
    pub prefix: bool,
    /// Whether a quoted phrase may be answered here (`NO PHRASE` clears it).
    pub phrase: bool,
    /// `SYNONYMS <set>`: alternatives a word is also answered by, in this field.
    pub synonyms: Option<Name>,
    /// `SNIPPET`: the field a best window may be taken from.
    pub snippet: bool,
}

/// What a `FROM SEARCH` asks of the search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchAsk {
    /// `MATCHES [PREFIX | FUZZY | INFIX] <query>` — the records, ranked.
    Matches {
        /// How the query's words are read.
        operator: SearchOperator,
        /// The query string.
        query: Box<Expr>,
    },
    /// `COMPLETE <beginning>` — the words the search holds that begin with it,
    /// ranked by how many records hold them.
    Complete {
        /// What was typed.
        beginning: Box<Expr>,
    },
}

/// How the words of a search query are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOperator {
    /// `MATCHES`: whole words, `OR`, `NOT`, quoted phrases and starred words.
    Words,
    /// `MATCHES PREFIX`: every word a beginning.
    Prefix,
    /// `MATCHES FUZZY`: every word within the edit budget.
    Fuzzy,
    /// `MATCHES INFIX`: every word a piece of a held word.
    Infix,
}
