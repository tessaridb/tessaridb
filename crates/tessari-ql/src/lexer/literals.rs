//! Lexing numbers, durations and byte strings.

use super::{Lexer, NANOS_PER_SECOND, hex_value, unit_nanos};
use crate::error::{Error, Result};
use crate::token::{Span, Token};
use tessari_types::{Duration, Number};

impl<'a> Lexer<'a> {
    pub(crate) fn number(&mut self, start: usize) -> Result<Token> {
        if self.peek() == Some('-') {
            self.advance('-');
        }
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X')) {
            return self.bytes(start);
        }

        self.digits();
        // A `.` begins a fraction only when a digit follows, so `1..10` stays a
        // range instead of becoming a float and a stray `.10`.
        let mut is_float = false;
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|next| next.is_ascii_digit()) {
            is_float = true;
            self.advance('.');
            self.digits();
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.advance('e');
            if matches!(self.peek(), Some('+' | '-')) {
                self.advance('-');
            }
            self.digits();
        }

        // Digits touching a letter are a duration, whatever the letter is — an
        // unknown unit is an error rather than a number beside a name, because
        // `5y` written for five years should say what is wrong with it instead
        // of parsing as two tokens and failing somewhere else.
        if !is_float && self.peek().is_some_and(|next| next.is_ascii_alphabetic()) {
            return self.duration(start);
        }

        let text = self.slice(start, self.position);
        let invalid = || Error::InvalidNumber {
            text: text.to_owned(),
            span: Span::new(start, self.position),
        };
        if is_float {
            let value: f64 = text.parse().map_err(|_| invalid())?;
            return Ok(Token::Number(Number::float(value)));
        }
        let value: i64 = text.parse().map_err(|_| invalid())?;
        Ok(Token::Number(Number::Integer(value)))
    }

    /// `1h30m`, `-500ms`. The digits before the first unit are already consumed.
    pub(crate) fn duration(&mut self, start: usize) -> Result<Token> {
        let negative = self
            .source
            .get(start..)
            .is_some_and(|rest| rest.starts_with('-'));
        let mut total: i128 = 0;
        let mut cursor = if negative {
            start.saturating_add(1)
        } else {
            start
        };

        loop {
            let digits_from = cursor;
            while self
                .source
                .get(cursor..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|character| character.is_ascii_digit())
            {
                cursor = cursor.saturating_add(1);
            }
            if digits_from == cursor {
                break;
            }
            let unit_from = cursor;
            while self
                .source
                .get(cursor..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|character| character.is_ascii_alphabetic())
            {
                cursor = cursor.saturating_add(1);
            }
            let amount: i128 = self
                .slice(digits_from, unit_from)
                .parse()
                .map_err(|_| self.invalid_duration(start, cursor))?;
            let unit = unit_nanos(self.slice(unit_from, cursor))
                .ok_or_else(|| self.invalid_duration(start, cursor))?;
            total = amount
                .checked_mul(unit)
                .and_then(|nanos| total.checked_add(nanos))
                .ok_or_else(|| self.invalid_duration(start, cursor))?;
        }

        self.position = cursor;
        if negative {
            total = total
                .checked_neg()
                .ok_or_else(|| self.invalid_duration(start, cursor))?;
        }
        // Normalised the way the value system stores time: a negative span
        // carries a negative second count and a positive remainder, so field-by
        // -field comparison is the same as comparing the quantity.
        let seconds = i64::try_from(total.div_euclid(NANOS_PER_SECOND))
            .map_err(|_| self.invalid_duration(start, cursor))?;
        let nanos = u32::try_from(total.rem_euclid(NANOS_PER_SECOND))
            .map_err(|_| self.invalid_duration(start, cursor))?;
        let value =
            Duration::new(seconds, nanos).ok_or_else(|| self.invalid_duration(start, cursor))?;
        Ok(Token::Duration(value))
    }

    /// `0x0a1b`. The `0` is unconsumed when this is entered.
    pub(crate) fn bytes(&mut self, start: usize) -> Result<Token> {
        self.advance('0');
        self.advance('x');
        let digits_from = self.position;
        while let Some(character) = self.peek() {
            if !character.is_ascii_alphanumeric() {
                break;
            }
            self.advance(character);
        }
        let digits = self.slice(digits_from, self.position);
        let span = Span::new(start, self.position);
        if digits.is_empty() || !digits.len().is_multiple_of(2) {
            return Err(Error::InvalidBytes {
                reason: "an odd number of digits",
                span,
            });
        }
        let mut bytes = Vec::with_capacity(digits.len() / 2);
        let raw = digits.as_bytes();
        for pair in raw.as_chunks::<2>().0 {
            let high = hex_value(pair.first().copied().unwrap_or(0));
            let low = hex_value(pair.get(1).copied().unwrap_or(0));
            match (high, low) {
                (Some(high), Some(low)) => bytes.push(high.saturating_mul(16).saturating_add(low)),
                _ => {
                    return Err(Error::InvalidBytes {
                        reason: "a digit that is not hexadecimal",
                        span,
                    });
                }
            }
        }
        Ok(Token::Bytes(bytes))
    }

    pub(crate) fn digits(&mut self) {
        while let Some(character) = self.peek() {
            if !character.is_ascii_digit() {
                break;
            }
            self.advance(character);
        }
    }
}
