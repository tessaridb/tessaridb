//! `HSCAN`, `SSCAN` and `ZSCAN`: one key's container read whole and answered
//! in one page with the cursor `0`, which a client reads as the walk being
//! over — `COUNT` is a hint the protocol lets a server answer past.

use crate::codec::Reply;
use crate::surface::Space;
use crate::surface::commands::Step;
use crate::surface::commands::arguments::integer;
use crate::surface::commands::containers::{bulk, first, hash, members, ranked};
use crate::surface::glob;
use crate::surface::replies;
use crate::surface::statement::{Binder, Planned};
use crate::surface::values;

const NAMES: &[&str] = &["HSCAN", "SSCAN", "ZSCAN"];

/// Whether `name` is one of these commands.
pub(super) fn serves(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Plan `name` when it walks one key's container.
pub(super) fn plan(
    name: &str,
    arguments: &[Vec<u8>],
    space: &Space,
    binder: &mut Binder,
) -> Result<Option<Step>, Reply> {
    if !serves(name) {
        return Ok(None);
    }
    let [key, cursor, options @ ..] = arguments else {
        return Err(replies::arity(name));
    };
    let cursor = u64::try_from(integer(cursor)?).map_err(|_| Reply::error("ERR invalid cursor"))?;
    let (mut pattern, mut values_too) = (None, name != "SSCAN");
    let mut words = options.iter();
    while let Some(word) = words.next() {
        match word.to_ascii_uppercase().as_slice() {
            b"MATCH" => pattern = Some(words.next().ok_or_else(replies::syntax)?.clone()),
            b"COUNT" => {
                let count = integer(words.next().ok_or_else(replies::syntax)?)?;
                if count < 1 {
                    return Err(replies::syntax());
                }
            }
            b"NOVALUES" if name == "HSCAN" => values_too = false,
            _ => return Err(replies::syntax()),
        }
    }
    let target = binder.key(space, key);
    let kind = name.to_owned();
    Ok(Some(Step::Statements(Planned::one(
        format!("GET {target}"),
        Box::new(move |outcomes| {
            // Only `0` is ever handed out, so any other cursor is a walk that
            // has already ended.
            if cursor != 0 {
                return page(Vec::new());
            }
            let wanted = |name: &[u8]| {
                pattern
                    .as_ref()
                    .is_none_or(|pattern| glob::matches(pattern, name))
            };
            let held = first(outcomes);
            let mut items = Vec::new();
            match kind.as_str() {
                "HSCAN" => match hash(held) {
                    Ok(fields) => {
                        for (field, value) in fields {
                            if wanted(field.as_bytes()) {
                                items.push(Reply::text(field));
                                if values_too {
                                    items.push(bulk(&value));
                                }
                            }
                        }
                    }
                    Err(reply) => return reply,
                },
                "SSCAN" => match members(held) {
                    Ok(set) => {
                        for member in set {
                            if values::rendered(&member).is_some_and(|bytes| wanted(&bytes)) {
                                items.push(bulk(&member));
                            }
                        }
                    }
                    Err(reply) => return reply,
                },
                _ => match ranked(held) {
                    Ok(pairs) => {
                        for (member, at) in pairs {
                            if wanted(member.as_bytes()) {
                                items.push(Reply::text(member));
                                // A scan answers text, a score included.
                                items.push(Reply::Bulk(values::float_text(at)));
                            }
                        }
                    }
                    Err(reply) => return reply,
                },
            }
            page(items)
        }),
    ))))
}

fn page(items: Vec<Reply>) -> Reply {
    Reply::Array(vec![Reply::text("0"), Reply::Array(items)])
}
