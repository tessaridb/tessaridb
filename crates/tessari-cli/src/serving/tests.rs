use super::join_token;

#[test]
fn a_join_token_is_the_64_digits_it_was_answered_as() {
    let written = "0f".repeat(32);
    assert_eq!(join_token(Some(&written)), Ok(Some([0x0f; 32])));
    for malformed in ["0f", &"0g".repeat(32), &"0f".repeat(33)] {
        let refused = join_token(Some(malformed)).expect_err(malformed);
        assert!(refused.contains("64 hexadecimal digits"), "{refused}");
    }
}
