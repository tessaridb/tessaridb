//! Which members of a ranking a range names — by rank, by score or by member
//! text — shared by `ZRANGE` and the commands that read or store a range.

use tessaridb::{Number, Value};

use crate::codec::Reply;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers;
use crate::surface::replies;
use crate::surface::statement::Binder;
use crate::surface::values;

/// One end of a score range: `5`, `(5` for excluded, `-inf`, `+inf`.
#[derive(Clone, Copy)]
pub(super) struct Bound {
    at: f64,
    excluded: bool,
}

impl Bound {
    pub(super) fn read(bytes: &[u8]) -> Result<Self, Reply> {
        let (excluded, digits) = match bytes.strip_prefix(b"(") {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let at = match digits.to_ascii_lowercase().as_slice() {
            b"-inf" => f64::NEG_INFINITY,
            b"+inf" | b"inf" => f64::INFINITY,
            _ => values::float(digits)
                .ok_or_else(|| Reply::error("ERR min or max is not a float"))?,
        };
        Ok(Self { at, excluded })
    }

    pub(super) fn below(self, score: f64) -> bool {
        if self.excluded {
            self.at < score
        } else {
            self.at <= score
        }
    }

    pub(super) fn above(self, score: f64) -> bool {
        if self.excluded {
            score < self.at
        } else {
            score <= self.at
        }
    }

    /// This end as a `SCORE` range end — `NONE` for an infinity on its own
    /// side — or `None` when an infinity on the other side names nothing.
    pub(super) fn written(self, low: bool, binder: &mut Binder) -> Option<String> {
        if self.at.is_infinite() {
            return (low == self.at.is_sign_negative()).then(|| "NONE".to_owned());
        }
        Some(ended(
            binder.bind(Value::Number(Number::float(self.at))),
            self.excluded,
        ))
    }
}

/// A range end as written, `EXCLUSIVE` when it is left out.
fn ended(value: String, excluded: bool) -> String {
    if excluded {
        format!("{value} EXCLUSIVE")
    } else {
        value
    }
}

/// One end of a member range: `-`, `+`, `[a` included, `(a` excluded.
#[derive(Clone)]
pub(super) enum Lex {
    Lowest,
    Highest,
    At { member: String, excluded: bool },
}

impl Lex {
    pub(super) fn read(bytes: &[u8]) -> Result<Self, Reply> {
        let invalid = || Reply::error("ERR min or max not valid string range item");
        match bytes {
            b"-" => Ok(Self::Lowest),
            b"+" => Ok(Self::Highest),
            [b'[' | b'(', rest @ ..] => Ok(Self::At {
                member: String::from_utf8(rest.to_vec()).map_err(|_| invalid())?,
                excluded: bytes.first() == Some(&b'('),
            }),
            _ => Err(invalid()),
        }
    }

    pub(super) fn below(&self, member: &str) -> bool {
        match self {
            Self::Lowest => true,
            Self::Highest => false,
            Self::At {
                member: at,
                excluded,
            } => {
                if *excluded {
                    at.as_str() < member
                } else {
                    at.as_str() <= member
                }
            }
        }
    }

    pub(super) fn above(&self, member: &str) -> bool {
        match self {
            Self::Lowest => false,
            Self::Highest => true,
            Self::At {
                member: at,
                excluded,
            } => {
                if *excluded {
                    member < at.as_str()
                } else {
                    member <= at.as_str()
                }
            }
        }
    }
}

impl Lex {
    /// This end as a `MEMBER` range end — `NONE` for `-` low or `+` high —
    /// or `None` when `+` low or `-` high names nothing.
    pub(super) fn written(&self, low: bool, binder: &mut Binder) -> Option<String> {
        match (self, low) {
            (Self::Lowest, true) | (Self::Highest, false) => Some("NONE".to_owned()),
            (Self::Lowest | Self::Highest, _) => None,
            (Self::At { member, excluded }, _) => {
                Some(ended(binder.bind(Value::String(member.clone())), *excluded))
            }
        }
    }
}

/// How a range names its members.
#[derive(Clone)]
enum By {
    Rank(i64, i64),
    Score(Bound, Bound),
    Lex(Lex, Lex),
}

/// A range as `ZRANGE` and its kin ask for it.
#[derive(Clone)]
pub(super) struct Range {
    by: By,
    reverse: bool,
    limit: Option<(i64, i64)>,
    pub(super) with_scores: bool,
}

impl Range {
    /// `ZRANGE`'s arguments after the key, or one of the older commands'
    /// (`ZRANGEBYSCORE`, `ZREVRANGE`, `ZRANGEBYLEX` …); `stored` refuses
    /// `WITHSCORES`, which `ZRANGESTORE` does not take.
    pub(super) fn read(name: &str, rest: &[Vec<u8>], stored: bool) -> Result<Self, Reply> {
        let [start, stop, options @ ..] = rest else {
            return Err(replies::arity(name));
        };
        let (mut by_score, mut by_lex) = (
            matches!(name, "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE"),
            matches!(name, "ZRANGEBYLEX" | "ZREVRANGEBYLEX"),
        );
        let mut reverse = name.starts_with("ZREV");
        let (mut with_scores, mut limit) = (false, None);
        let mut words = options.iter();
        while let Some(word) = words.next() {
            match word.to_ascii_uppercase().as_slice() {
                b"WITHSCORES" if !stored && !by_lex => with_scores = true,
                b"BYSCORE" if name == "ZRANGE" => by_score = true,
                b"BYLEX" if name == "ZRANGE" => by_lex = true,
                b"REV" if name == "ZRANGE" => reverse = true,
                b"LIMIT" => {
                    let offset = integer(words.next().ok_or_else(replies::syntax)?)?;
                    let count = integer(words.next().ok_or_else(replies::syntax)?)?;
                    limit = Some((offset, count));
                }
                _ => return Err(replies::syntax()),
            }
        }
        if by_lex && with_scores {
            return Err(Reply::error(
                "ERR syntax error, WITHSCORES not supported in combination with BYLEX",
            ));
        }
        // A reversed range names its high end first.
        let (low, high) = if reverse {
            (stop, start)
        } else {
            (start, stop)
        };
        let by = if by_score {
            By::Score(Bound::read(low)?, Bound::read(high)?)
        } else if by_lex {
            By::Lex(Lex::read(low)?, Lex::read(high)?)
        } else if limit.is_some() {
            return Err(Reply::error(
                "ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX",
            ));
        } else {
            By::Rank(integer(start)?, integer(stop)?)
        };
        Ok(Self {
            by,
            reverse,
            limit,
            with_scores,
        })
    }

    /// The members the range names, in the order it answers them.
    pub(super) fn select(&self, mut pairs: Vec<(String, f64)>) -> Vec<(String, f64)> {
        if self.reverse {
            pairs.reverse();
        }
        let within: Vec<(String, f64)> = match &self.by {
            By::Rank(start, stop) => {
                return match containers::span(pairs.len(), *start, *stop) {
                    Some((from, to)) => pairs.get(from..=to).unwrap_or(&[]).to_vec(),
                    None => Vec::new(),
                };
            }
            By::Score(low, high) => pairs
                .into_iter()
                .filter(|(_, at)| low.below(*at) && high.above(*at))
                .collect(),
            By::Lex(low, high) => pairs
                .into_iter()
                .filter(|(member, _)| low.below(member) && high.above(member))
                .collect(),
        };
        match self.limit {
            Some((offset, count)) => {
                let offset = usize::try_from(offset).unwrap_or(usize::MAX);
                let count = usize::try_from(count).unwrap_or(usize::MAX);
                within.into_iter().skip(offset).take(count).collect()
            }
            None => within,
        }
    }
}
