//! What the lexer must get right, and the three places it would get it wrong
//! quietly.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{Error, Keyword, Punct, Token, tokenize};
use tessari_types::{Duration, Number};

fn tokens(source: &str) -> Vec<Token> {
    tokenize(source)
        .unwrap()
        .into_iter()
        .map(|spanned| spanned.token)
        .collect()
}

#[test]
fn a_definition_statement_lexes_into_its_parts() {
    assert_eq!(
        tokens("DEFINE INDEX by_email ON users FIELDS email UNIQUE;"),
        vec![
            Token::Keyword(Keyword::Define),
            Token::Keyword(Keyword::Index),
            Token::Ident("by_email".to_owned()),
            Token::Keyword(Keyword::On),
            Token::Ident("users".to_owned()),
            Token::Keyword(Keyword::Fields),
            Token::Ident("email".to_owned()),
            Token::Keyword(Keyword::Unique),
            Token::Punct(Punct::Semicolon),
        ]
    );
}

#[test]
fn keywords_are_case_insensitive_and_names_are_not() {
    assert_eq!(
        tokens("select Users from users"),
        vec![
            Token::Keyword(Keyword::Select),
            Token::Ident("Users".to_owned()),
            Token::Keyword(Keyword::From),
            Token::Ident("users".to_owned()),
        ]
    );
}

#[test]
fn a_range_is_not_two_numbers_and_a_float_is_not_a_range() {
    // Without the rule that a `.` begins a fraction only when a digit follows,
    // `1..10` lexes as the float `1.` and then `.10`, and a range silently
    // becomes something else.
    assert_eq!(
        tokens("1..10"),
        vec![
            Token::Number(Number::Integer(1)),
            Token::Punct(Punct::DotDot),
            Token::Number(Number::Integer(10)),
        ]
    );
    assert_eq!(
        tokens("1..=10"),
        vec![
            Token::Number(Number::Integer(1)),
            Token::Punct(Punct::DotDotEquals),
            Token::Number(Number::Integer(10)),
        ]
    );
    assert_eq!(tokens("1.5"), vec![Token::Number(Number::float(1.5))]);
    assert_eq!(
        tokens("orders.users"),
        vec![
            Token::Ident("orders".to_owned()),
            Token::Punct(Punct::Dot),
            Token::Ident("users".to_owned()),
        ]
    );
}

#[test]
fn numbers_carry_the_kind_they_were_written_in() {
    assert_eq!(tokens("42"), vec![Token::Number(Number::Integer(42))]);
    assert_eq!(tokens("-7"), vec![Token::Number(Number::Integer(-7))]);
    assert_eq!(tokens("1e10"), vec![Token::Number(Number::float(1e10))]);
    assert_eq!(tokens("1.5e-3"), vec![Token::Number(Number::float(1.5e-3))]);

    // The exact-decimal marker is a keyword, so the number it applies to arrives
    // as an ordinary number and the parser makes it exact.
    assert_eq!(
        tokens("dec 12.34"),
        vec![
            Token::Keyword(Keyword::Dec),
            Token::Number(Number::float(12.34)),
        ]
    );
}

#[test]
fn a_number_too_large_for_the_type_is_refused_rather_than_wrapped() {
    let error = tokenize("99999999999999999999").unwrap_err();
    assert!(matches!(error, Error::InvalidNumber { .. }), "{error}");
}

#[test]
fn durations_compose_and_normalise_the_way_the_value_system_stores_them() {
    assert_eq!(
        tokens("2s"),
        vec![Token::Duration(Duration::from_seconds(2))]
    );
    assert_eq!(
        tokens("1h30m"),
        vec![Token::Duration(Duration::from_seconds(5400))]
    );

    // A negative span carries a negative second count and a positive remainder,
    // which is what makes field-by-field comparison the same as comparing the
    // quantity.
    let Token::Duration(negative) = tokens("-500ms").first().cloned().unwrap() else {
        panic!("expected a duration");
    };
    assert_eq!(negative.seconds(), -1);
    assert_eq!(negative.nanos(), 500_000_000);
}

#[test]
fn a_duration_with_an_unknown_unit_is_refused() {
    let error = tokenize("5y").unwrap_err();
    assert!(matches!(error, Error::InvalidDuration { .. }), "{error}");
}

#[test]
fn a_number_beside_a_name_is_not_confused_with_a_duration() {
    // `1e10` is a float, not one exa-something: `e` is not a duration unit.
    assert_eq!(tokens("1e10"), vec![Token::Number(Number::float(1e10))]);
}

#[test]
fn strings_take_both_quotes_and_resolve_their_escapes() {
    assert_eq!(tokens("'ada'"), vec![Token::Str("ada".to_owned())]);
    assert_eq!(tokens("\"ada\""), vec![Token::Str("ada".to_owned())]);
    assert_eq!(
        tokens(r"'a\'b\\c\nd'"),
        vec![Token::Str("a'b\\c\nd".to_owned())]
    );
    assert_eq!(tokens("'привет'"), vec![Token::Str("привет".to_owned())]);
}

#[test]
fn an_unknown_escape_is_refused_rather_than_silently_dropping_the_backslash() {
    // Passing `\d` through as `d` would store a value that differs from the one
    // written, with nothing anywhere to notice.
    let error = tokenize(r"'a\db'").unwrap_err();
    assert!(
        matches!(error, Error::InvalidEscape { found: 'd', .. }),
        "{error}"
    );
}

#[test]
fn an_unterminated_string_is_refused_and_points_at_where_it_began() {
    let error = tokenize("SELECT 'ada").unwrap_err();
    assert!(matches!(error, Error::UnterminatedString { .. }), "{error}");
    assert_eq!(error.span().start, 7);
}

#[test]
fn byte_literals_are_whole_bytes_of_hexadecimal() {
    assert_eq!(tokens("0x0a1b"), vec![Token::Bytes(vec![0x0a, 0x1b])]);
    assert_eq!(tokens("0xFF"), vec![Token::Bytes(vec![0xff])]);

    for bad in ["0x0", "0xzz", "0x"] {
        let error = tokenize(bad).unwrap_err();
        assert!(
            matches!(error, Error::InvalidBytes { .. }),
            "{bad} produced {error}"
        );
    }
}

#[test]
fn a_record_literal_lexes_as_a_name_a_colon_and_an_identity() {
    assert_eq!(
        tokens("users:1"),
        vec![
            Token::Ident("users".to_owned()),
            Token::Punct(Punct::Colon),
            Token::Number(Number::Integer(1)),
        ]
    );
    assert_eq!(
        tokens("users:'ada'"),
        vec![
            Token::Ident("users".to_owned()),
            Token::Punct(Punct::Colon),
            Token::Str("ada".to_owned()),
        ]
    );
}

#[test]
fn comments_and_whitespace_carry_no_tokens() {
    assert_eq!(
        tokens("-- a note\nSELECT * FROM users; -- another"),
        vec![
            Token::Keyword(Keyword::Select),
            Token::Punct(Punct::Star),
            Token::Keyword(Keyword::From),
            Token::Ident("users".to_owned()),
            Token::Punct(Punct::Semicolon),
        ]
    );
    assert!(tokens("   \n\t  ").is_empty());
    assert!(tokens("").is_empty());
}

#[test]
fn a_character_the_language_does_not_use_is_refused_with_its_position() {
    let error = tokenize("SELECT # FROM users").unwrap_err();
    assert!(
        matches!(error, Error::UnexpectedCharacter { found: '#', .. }),
        "{error}"
    );
    assert_eq!(error.span().start, 7);
}

#[test]
fn every_token_spans_the_text_it_came_from() {
    let source = "SET sessions:'abc' = 42;";
    for spanned in tokenize(source).unwrap() {
        let text = source.get(spanned.span.start..spanned.span.end);
        assert!(text.is_some(), "{spanned:?} spans outside the source");
        assert!(!spanned.span.is_empty(), "{spanned:?} spans nothing");
    }
}

#[test]
fn a_whole_key_value_script_lexes() {
    let source = "\
        USE NAMESPACE prod DATABASE orders;\n\
        BEGIN;\n\
        CREATE users:1 = { name: 'ada', tags: ['a', 'b'] };\n\
        SET sessions:'abc' = datetime '2026-09-01T00:00:00Z';\n\
        KEYS FROM sessions RANGE 'a'..'m';\n\
        COMMIT;";
    let lexed = tokenize(source).unwrap();
    assert!(lexed.len() > 30, "lexed {} tokens", lexed.len());
    assert_eq!(
        lexed.last().map(|spanned| spanned.token.clone()),
        Some(Token::Punct(Punct::Semicolon))
    );
}

#[test]
fn the_set_keyword_serves_both_the_verb_and_the_set_literal() {
    // One keyword, two roles, told apart by whether a `[` follows. The lexer
    // does not decide that; it must only be consistent about the token.
    assert_eq!(tokens("SET [1, 2]").first(), tokens("SET x = 1").first(),);
}

#[test]
fn an_identifier_never_carries_a_path_delimiter() {
    // `Path::new` trusts its caller not to hand it a name containing `.`, `[` or
    // `]`, and the parser is one of two callers. This is that claim: the lexer
    // splits every delimiter out into its own token, so an `Ident` cannot hold
    // one, and a path built from idents round-trips through its spelling.
    assert_eq!(
        tokens("address.city[0]"),
        vec![
            Token::Ident("address".to_owned()),
            Token::Punct(Punct::Dot),
            Token::Ident("city".to_owned()),
            Token::Punct(Punct::BracketOpen),
            Token::Number(Number::Integer(0)),
            Token::Punct(Punct::BracketClose),
        ]
    );
}
