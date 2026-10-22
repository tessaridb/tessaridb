//! The glob a `KEYS` or `SCAN … MATCH` pattern is: `*`, `?`, `[set]`,
//! `[^set]`, `[a-z]` and `\` escaping the next byte, over bytes.

/// Whether `text` matches `pattern`.
///
/// Iterative, remembering only the last `*`: a pattern of many stars against
/// a long key costs its length times the key's, never exponential time.
pub(crate) fn matches(pattern: &[u8], text: &[u8]) -> bool {
    let (mut at, mut read) = (0_usize, 0_usize);
    let mut star: Option<(usize, usize)> = None;
    while read < text.len() {
        match pattern.get(at) {
            Some(b'*') => {
                star = Some((at, read));
                at = at.saturating_add(1);
                continue;
            }
            Some(_) => {
                if let Some(next) = one(pattern, at, text[read]) {
                    at = next;
                    read = read.saturating_add(1);
                    continue;
                }
            }
            None => {}
        }
        // A mismatch: let the last star take one more byte, or fail.
        let Some((star_at, star_read)) = star else {
            return false;
        };
        let taken = star_read.saturating_add(1);
        star = Some((star_at, taken));
        at = star_at.saturating_add(1);
        read = taken;
    }
    pattern[at.min(pattern.len())..]
        .iter()
        .all(|byte| *byte == b'*')
}

/// Match one byte against the pattern element at `at`: the index after the
/// element when it matches, `None` when it does not.
fn one(pattern: &[u8], at: usize, byte: u8) -> Option<usize> {
    match pattern[at] {
        b'?' => Some(at.saturating_add(1)),
        b'\\' if at.saturating_add(1) < pattern.len() => {
            (pattern[at.saturating_add(1)] == byte).then_some(at.saturating_add(2))
        }
        b'[' => set(pattern, at, byte),
        literal => (literal == byte).then_some(at.saturating_add(1)),
    }
}

/// A `[…]` set at `at`; an unclosed one runs to the end of the pattern.
fn set(pattern: &[u8], at: usize, byte: u8) -> Option<usize> {
    let mut index = at.saturating_add(1);
    let negated = pattern.get(index) == Some(&b'^');
    if negated {
        index = index.saturating_add(1);
    }
    let mut found = false;
    while let Some(&element) = pattern.get(index) {
        if element == b']' {
            break;
        }
        if element == b'\\' && index.saturating_add(1) < pattern.len() {
            index = index.saturating_add(1);
            found |= pattern[index] == byte;
        } else if pattern.get(index.saturating_add(1)) == Some(&b'-')
            && let Some(&end) = pattern.get(index.saturating_add(2))
            && end != b']'
        {
            let (low, high) = if element <= end {
                (element, end)
            } else {
                (end, element)
            };
            found |= (low..=high).contains(&byte);
            index = index.saturating_add(2);
        } else {
            found |= element == byte;
        }
        index = index.saturating_add(1);
    }
    (found != negated).then_some(index.saturating_add(1))
}

/// The bytes every match starts with: the pattern up to its first special
/// byte, which a walk can seek to rather than read every key.
pub(crate) fn literal_prefix(pattern: &[u8]) -> &[u8] {
    let end = pattern
        .iter()
        .position(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
        .unwrap_or(pattern.len());
    &pattern[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_matches_as_the_protocol_documents() {
        let cases: &[(&str, &str, bool)] = &[
            ("*", "anything", true),
            ("*", "", true),
            ("h?llo", "hello", true),
            ("h?llo", "hllo", false),
            ("h*llo", "heeeello", true),
            ("h[ae]llo", "hallo", true),
            ("h[ae]llo", "hillo", false),
            ("h[^e]llo", "hallo", true),
            ("h[^e]llo", "hello", false),
            ("h[a-b]llo", "hbllo", true),
            ("h[a-b]llo", "hcllo", false),
            ("h\\*llo", "h*llo", true),
            ("h\\*llo", "hello", false),
            ("user:*:name", "user:42:name", true),
            ("user:*:name", "user:42:mail", false),
            ("a*b*c", "aXbXc", true),
            ("a*b*c", "aXbX", false),
            ("abc", "abcd", false),
        ];
        for (pattern, text, expected) in cases {
            assert_eq!(
                matches(pattern.as_bytes(), text.as_bytes()),
                *expected,
                "{pattern} against {text}"
            );
        }
    }

    #[test]
    fn many_stars_against_a_long_key_end_quickly() {
        let pattern = b"*a*a*a*a*a*a*a*a*a*a*b";
        let text = vec![b'a'; 10_000];
        assert!(!matches(pattern, &text));
    }

    #[test]
    fn the_literal_prefix_stops_at_the_first_special_byte() {
        assert_eq!(literal_prefix(b"user:*"), b"user:");
        assert_eq!(literal_prefix(b"*"), b"");
        assert_eq!(literal_prefix(b"exact"), b"exact");
        assert_eq!(literal_prefix(b"a?b"), b"a");
    }
}
