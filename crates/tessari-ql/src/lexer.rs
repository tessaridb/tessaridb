//! Turning a script into tokens.
//!
//! Three decisions here are load-bearing and each exists because its opposite
//! fails quietly rather than loudly.
//!
//! **A `.` only begins a fraction when a digit follows it.** Without that rule
//! `1..10` lexes as the float `1.` followed by `.10`, and a range silently
//! becomes two numbers.
//!
//! **Digits touching a letter are a duration**, so `1h30m` is one token and `1m`
//! is a minute rather than the number one beside a name. An unknown unit is an
//! error rather than two tokens, so `5y` written for five years says what is
//! wrong with it instead of failing somewhere else. Nothing in this grammar puts
//! a name directly against a number, so the rule costs nothing.
//!
//! **An unknown backslash escape is refused.** Passing it through would make
//! `\d` store a `d` and lose the backslash, so the value read back would differ
//! from the value written, with nothing to notice it.

mod literals;

use crate::error::{Error, Result};
use crate::token::{Keyword, Punct, Span, Spanned, Token};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// Read `source` into tokens.
///
/// # Errors
///
/// Returns the first lexical failure, with the span it occurred at.
pub fn tokenize(source: &str) -> Result<Vec<Spanned>> {
    Lexer::new(source).run()
}

struct Lexer<'a> {
    source: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    const fn new(source: &'a str) -> Self {
        Self {
            source,
            position: 0,
        }
    }

    fn run(mut self) -> Result<Vec<Spanned>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_ignorable();
            let start = self.position;
            let Some(character) = self.peek() else {
                return Ok(tokens);
            };
            let token = match character {
                '0'..='9' => self.number(start)?,
                '-' if self.peek_at(1).is_some_and(|next| next.is_ascii_digit()) => {
                    self.number(start)?
                }
                '\'' | '"' => Token::Str(self.string(character, start)?),
                '$' => self.parameter(start)?,
                character if is_name_start(character) => self.word(),
                _ => Token::Punct(self.punctuation(character, start)?),
            };
            tokens.push(Spanned::new(token, Span::new(start, self.position)));
        }
    }

    /// Whitespace, and comments, which run to the end of their line.
    fn skip_ignorable(&mut self) {
        loop {
            match self.peek() {
                Some(character) if character.is_whitespace() => self.advance(character),
                Some('-') if self.peek_at(1) == Some('-') => {
                    while let Some(character) = self.peek() {
                        if character == '\n' {
                            break;
                        }
                        self.advance(character);
                    }
                }
                _ => return,
            }
        }
    }

    fn word(&mut self) -> Token {
        let start = self.position;
        while let Some(character) = self.peek() {
            if !is_name_continue(character) {
                break;
            }
            self.advance(character);
        }
        let word = self.slice(start, self.position);
        Keyword::from_word(word).map_or_else(|| Token::Ident(word.to_owned()), Token::Keyword)
    }

    /// `$name` — a parameter the caller binds a value to.
    ///
    /// A bare `$` is refused rather than read as punctuation: it is written by
    /// somebody who meant a parameter, and the alternative is a parse failure
    /// several tokens later pointing at something that is not the mistake.
    fn parameter(&mut self, start: usize) -> Result<Token> {
        self.advance('$');
        let from = self.position;
        while let Some(character) = self.peek() {
            if !is_name_continue(character) {
                break;
            }
            self.advance(character);
        }
        if self.position == from {
            return Err(Error::UnexpectedCharacter {
                found: '$',
                span: Span::new(start, self.position),
            });
        }
        Ok(Token::Parameter(self.slice(from, self.position).to_owned()))
    }

    fn invalid_duration(&self, start: usize, end: usize) -> Error {
        Error::InvalidDuration {
            text: self.slice(start, end).to_owned(),
            span: Span::new(start, end),
        }
    }

    fn string(&mut self, quote: char, start: usize) -> Result<String> {
        self.advance(quote);
        let mut text = String::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(Error::UnterminatedString {
                    span: Span::new(start, self.position),
                });
            };
            self.advance(character);
            match character {
                character if character == quote => return Ok(text),
                '\\' => text.push(self.escape()?),
                character => text.push(character),
            }
        }
    }

    fn escape(&mut self) -> Result<char> {
        let start = self.position.saturating_sub(1);
        let Some(character) = self.peek() else {
            return Err(Error::UnterminatedString {
                span: Span::new(start, self.position),
            });
        };
        self.advance(character);
        match character {
            'n' => Ok('\n'),
            't' => Ok('\t'),
            'r' => Ok('\r'),
            '0' => Ok('\0'),
            '\\' | '\'' | '"' => Ok(character),
            found => Err(Error::InvalidEscape {
                found,
                span: Span::new(start, self.position),
            }),
        }
    }

    fn punctuation(&mut self, character: char, start: usize) -> Result<Punct> {
        self.advance(character);
        let punct = match character {
            ';' => Punct::Semicolon,
            ',' => Punct::Comma,
            ':' if self.peek() == Some(':') => {
                self.advance(':');
                Punct::ColonColon
            }
            ':' => Punct::Colon,
            '=' => Punct::Equals,
            '*' => Punct::Star,
            '{' => Punct::BraceOpen,
            '}' => Punct::BraceClose,
            '[' => Punct::BracketOpen,
            ']' => Punct::BracketClose,
            // `--` was consumed as a comment and `-<digit>` as a number before
            // reaching here, so a `-` left over is an arrow or nothing.
            '-' if self.peek() == Some('>') => {
                self.advance('>');
                Punct::ArrowRight
            }
            '<' if self.peek() == Some('-') => {
                self.advance('-');
                Punct::ArrowLeft
            }
            // `<-` is taken above, so what is left is a comparison. The order
            // matters: reading `<` first would make every backward traversal
            // lex as "below, then a minus".
            '<' if self.peek() == Some('=') => {
                self.advance('=');
                Punct::LessOrEqual
            }
            '<' => Punct::Less,
            '>' if self.peek() == Some('=') => {
                self.advance('=');
                Punct::GreaterOrEqual
            }
            '>' => Punct::Greater,
            '!' if self.peek() == Some('=') => {
                self.advance('=');
                Punct::NotEquals
            }
            '+' => Punct::Plus,
            // `--` was consumed as a comment and `-<digit>` as a number before
            // reaching here, and `->` is taken above, so what is left is
            // subtraction or a negation of something that is not a literal.
            '-' => Punct::Minus,
            '/' => Punct::Slash,
            '%' => Punct::Percent,
            // Two characters or none. A lone `?` is not a placeholder in this
            // language — a value is written `$name` — so reading one as the
            // start of something would give a worse error further along than
            // refusing it here does.
            '?' => {
                if self.peek() != Some('?') {
                    return Err(Error::UnexpectedCharacter {
                        found: '?',
                        span: Span::new(start, self.position),
                    });
                }
                self.advance('?');
                Punct::Coalesce
            }
            '|' => Punct::Pipe,
            '(' => Punct::ParenOpen,
            ')' => Punct::ParenClose,
            '.' => {
                if self.peek() != Some('.') {
                    return Ok(Punct::Dot);
                }
                self.advance('.');
                if self.peek() == Some('=') {
                    self.advance('=');
                    return Ok(Punct::DotDotEquals);
                }
                return Ok(Punct::DotDot);
            }
            found => {
                return Err(Error::UnexpectedCharacter {
                    found,
                    span: Span::new(start, self.position),
                });
            }
        };
        Ok(punct)
    }

    fn peek(&self) -> Option<char> {
        self.source.get(self.position..)?.chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.source.get(self.position..)?.chars().nth(offset)
    }

    fn advance(&mut self, character: char) {
        self.position = self.position.saturating_add(character.len_utf8());
    }

    fn slice(&self, start: usize, end: usize) -> &'a str {
        self.source.get(start..end).unwrap_or_default()
    }
}

const fn is_name_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

const fn is_name_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

const fn unit_nanos(unit: &str) -> Option<i128> {
    match unit.as_bytes() {
        b"ns" => Some(1),
        b"us" => Some(1_000),
        b"ms" => Some(1_000_000),
        b"s" => Some(NANOS_PER_SECOND),
        b"m" => Some(60 * NANOS_PER_SECOND),
        b"h" => Some(3_600 * NANOS_PER_SECOND),
        b"d" => Some(86_400 * NANOS_PER_SECOND),
        b"w" => Some(604_800 * NANOS_PER_SECOND),
        _ => None,
    }
}

const fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit.saturating_sub(b'0')),
        b'a'..=b'f' => Some(digit.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(digit.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}
