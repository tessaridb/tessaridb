use super::*;

/// The string a name is unique against.
///
/// The level tag is what keeps the levels apart: without it a namespace named
/// `5/orders` and the database `orders` inside namespace `5` would qualify to
/// the same string, and one would be refused as a duplicate of something
/// unrelated. Parent ids are numeric, so a `/` inside a name can never be
/// mistaken for a separator that precedes it.
/// Read a qualified name back into the level and parent ids it was built from.
///
/// The inverse of [`qualify`], and it lives beside it for the reason
/// `Reach::of` and `Reach::parts` live beside each other: one format written in
/// two places drifts, and the copy that drifts is the one nobody is reading.
///
/// `None` for anything this build did not write — an unknown tag, a missing
/// separator, a parent that is not a number. A caller gets *cannot tell* rather
/// than a guess, because the caller asking is the replication filter and its
/// answer to *cannot tell* is to withhold.
///
/// The name itself is deliberately not returned. The one caller needs the
/// tenancy and nothing else, and handing back a borrowed name would invite a
/// second caller to compare strings the catalog compares by id.
pub(super) fn parse_qualified(qualified: &str) -> Option<(Level, Vec<u32>)> {
    let (tag, rest) = qualified.split_once(':')?;
    let level = Level::from_tag(tag)?;
    let mut parents = Vec::new();
    let mut rest = rest;
    // A name may itself contain '/', which is why the parents are counted from
    // the left rather than split from the right: every parent is a number, and
    // the first segment that is not one is where the name begins.
    while let Some((head, tail)) = rest.split_once('/') {
        let Ok(parent) = head.parse::<u32>() else {
            break;
        };
        parents.push(parent);
        rest = tail;
    }
    Some((level, parents))
}
