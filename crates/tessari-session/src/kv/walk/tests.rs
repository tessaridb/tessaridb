use super::successor;

#[test]
fn the_successor_moves_the_last_character_one_code_point_on() {
    assert_eq!(successor("user:42:").as_deref(), Some("user:42;"));
    assert_eq!(successor("a").as_deref(), Some("b"));
}

#[test]
fn the_successor_steps_over_the_surrogate_gap_and_past_the_last_code_point() {
    assert_eq!(successor("\u{d7ff}").as_deref(), Some("\u{e000}"));
    assert_eq!(successor("a\u{10ffff}").as_deref(), Some("b"));
    assert_eq!(successor("\u{10ffff}"), None);
}
