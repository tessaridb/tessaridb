use super::{DEFAULT, Format, filter, format};

#[test]
fn nothing_asked_reports_at_the_default_level() {
    let (filter, unread) = filter(None);
    assert_eq!(filter.to_string(), DEFAULT);
    assert!(unread.is_none());
    let (blank, unread) = super::filter(Some("  "));
    assert_eq!(blank.to_string(), DEFAULT);
    assert!(unread.is_none());
}

#[test]
fn a_level_and_per_module_directives_are_both_read() {
    let (level, unread) = filter(Some("WARN"));
    assert_eq!(level.to_string(), "warn");
    assert!(unread.is_none());
    let (directives, unread) = filter(Some("info,tessari_wire=debug"));
    assert!(unread.is_none());
    let written = directives.to_string();
    assert!(
        written.contains("tessari_wire=debug") && written.contains("info"),
        "{written}"
    );
}

#[test]
fn an_unreadable_filter_falls_back_and_says_what_it_could_not_read() {
    let (fallback, unread) = filter(Some("loud=please=now"));
    assert_eq!(fallback.to_string(), DEFAULT);
    assert_eq!(unread.as_deref(), Some("loud=please=now"));
}

#[test]
fn the_format_is_text_unless_json_is_asked_for() {
    assert_eq!(format(None), (Format::Text, None));
    assert_eq!(format(Some("text")), (Format::Text, None));
    assert_eq!(format(Some(" JSON ")), (Format::Json, None));
    assert_eq!(
        format(Some("yaml")),
        (Format::Text, Some("yaml".to_owned()))
    );
}
