//! How an answer's outcomes are written into a frame body and read back.

use super::{Answer, Correction, Exact, Names, Remark, Suggested, put_names, tag};
use crate::error::{Error, Result};
use crate::frame::{put_bytes, put_text, put_u32, take_bytes, take_text, take_u32};
use tessari_encoding::{decode_payload, encode_payload};
use tessari_session::{AccessPath, Outcome, Suggestion};
use tessari_types::{RecordId, TableId};

/// The outcome itself, from its tag onward — everything the length counts.
#[cfg(feature = "server")]
pub(crate) fn encode_outcome_body(outcome: &Outcome, names: &Names) -> Vec<u8> {
    let mut body = Vec::new();
    match outcome {
        Outcome::Done => body.push(tag::DONE),
        // The notes go **last**, after the records, and that placement is the
        // whole of their compatibility story. An outcome is length-prefixed and
        // the reader advances by the declared length rather than by what it
        // consumed, so bytes appended at the end are bytes an older client steps
        // over — the same mechanism that lets it survive an outcome tag it has
        // never heard of. Put anywhere else they would shift the offsets of
        // fields an older client does know how to read.
        Outcome::Records {
            records,
            plan,
            notes,
            suggestion,
            only,
        } => {
            body.push(tag::RECORDS);
            body.push(path_tag(plan.access));
            put_names(&mut body, names);
            put_u32(&mut body, u32::try_from(records.len()).unwrap_or(u32::MAX));
            for (id, value) in records {
                put_text(&mut body, &spell(id));
                put_bytes(&mut body, encode_payload(value).as_slice());
            }
            // A kind and a rendered message rather than the typed note. `Answer`
            // is deliberately not `Outcome` — a record id on this wire is text
            // too — and a client's two uses are to group by the kind and to show
            // the message, both of which the store already writes.
            put_u32(&mut body, u32::try_from(notes.len()).unwrap_or(u32::MAX));
            for note in notes {
                put_text(&mut body, note.kind());
                put_text(&mut body, &note.message());
            }
            // After the notes, for the reason the notes are after the records:
            // a client that stops before it reads `false`, which is the truth
            // about every read written by somebody who has never heard of
            // `ONLY`.
            body.push(u8::from(*only));
            // And exactness last, where the newest field goes — but read the
            // note on `Exact` before assuming the usual absent-means-default
            // rule applies to it. It does not, and this is the one field on this
            // wire for which it must not.
            body.push(u8::from(!plan.exact.is_exact()));
            put_text(&mut body, plan.exact.reason().unwrap_or_default());
            // The suggestion last, as the newest field. Its three states get
            // three distinct byte values rather than a flag plus an empty list,
            // because the difference this field exists to carry is exactly the
            // one a flag would lose: `0` is *no dictionary was asked*, `1` is *a
            // dictionary was asked and holds every term*, and `2` is *these
            // terms it does not*. A client that reads `0` where it meant `1`
            // reports a confident negative nobody checked.
            //
            // Which also decides what an older client's silence means. It stops
            // before this byte and so reads no suggestion at all — the honest
            // outcome, and the reason absent is `0` rather than any of the three
            // being the implicit default.
            match suggestion {
                None => body.push(0),
                Some(Suggestion::NothingNearer) => body.push(1),
                Some(Suggestion::DidYouMean(nearest)) => {
                    body.push(2);
                    put_u32(&mut body, u32::try_from(nearest.len()).unwrap_or(u32::MAX));
                    for correction in nearest {
                        put_text(&mut body, &correction.typed);
                        put_text(&mut body, &correction.instead);
                    }
                }
            }
        }
        Outcome::Value(held) => {
            body.push(tag::VALUE);
            put_names(&mut body, names);
            put_bytes(&mut body, encode_payload(held).as_slice());
        }
        Outcome::Keys(keys) => {
            body.push(tag::KEYS);
            put_u32(&mut body, u32::try_from(keys.len()).unwrap_or(u32::MAX));
            for key in keys {
                put_text(&mut body, &spell(key));
            }
        }
        Outcome::Removed { count } => {
            body.push(tag::REMOVED);
            body.extend_from_slice(&count.to_be_bytes());
        }
        _ => body.push(tag::UNKNOWN),
    }
    body
}

/// One outcome, from its tag onward, in exactly the bytes its length claimed.
pub(crate) fn decode_outcome_body(body: &[u8]) -> Result<Answer> {
    let tag = body.first().copied().ok_or(Error::Malformed)?;
    let mut at = 1_usize;
    let answer = match tag {
        tag::DONE => Answer::Done,
        tag::RECORDS => {
            let path = path_name(body.get(at).copied().ok_or(Error::Malformed)?).to_owned();
            at = at.saturating_add(1);
            let (names, next) = take_names(body, at)?;
            at = next;
            let (count, next) = take_u32(body, at)?;
            at = next;
            let mut records = Vec::new();
            for _ in 0..count {
                let (id, next) = take_text(body, at)?;
                let (bytes, next) = take_bytes(body, next)?;
                at = next;
                records.push((id, decode_payload(&bytes)?));
            }
            // Only if there are bytes left. A node older than this build sends
            // a body that ends here, and that is a node with nothing to say
            // rather than a short read — the one direction the length prefix
            // does not cover on its own.
            let notes = if at < body.len() {
                let (count, next) = take_u32(body, at)?;
                at = next;
                let mut notes = Vec::new();
                for _ in 0..count {
                    let (kind, next) = take_text(body, at)?;
                    let (message, next) = take_text(body, next)?;
                    at = next;
                    notes.push(Remark { kind, message });
                }
                notes
            } else {
                Vec::new()
            };
            // Same rule one field further along: absent means `false`, which is
            // what an older node's every read was.
            let only = body.get(at).copied().unwrap_or(0) != 0;
            at = at.saturating_add(1);
            // And here the rule stops. An absent exactness byte does **not**
            // mean the answer was exact: it means the node never said, and
            // reading it as `true` would put a claim in an older node's mouth on
            // the one property that exists precisely so it is never inferred.
            // `None` is a third answer and a caller has to handle it.
            let exact = if at < body.len() {
                let approximate = body.get(at).copied().ok_or(Error::Malformed)? != 0;
                let (reason, next) = take_text(body, at.saturating_add(1))?;
                at = next;
                Some(if approximate {
                    Exact::No { reason }
                } else {
                    Exact::Yes
                })
            } else {
                None
            };
            // And the same rule again, one field further along, because the
            // reason for it is the same: silence here is a node that never had
            // the question, not a node reporting that nothing was near.
            let suggestion = if at < body.len() {
                let state = body.get(at).copied().ok_or(Error::Malformed)?;
                at = at.saturating_add(1);
                match state {
                    0 => Some(Suggested::NotSought),
                    1 => Some(Suggested::NothingNearer),
                    2 => {
                        let (count, next) = take_u32(body, at)?;
                        at = next;
                        let mut corrections = Vec::new();
                        for _ in 0..count {
                            let (typed, next) = take_text(body, at)?;
                            let (instead, next) = take_text(body, next)?;
                            at = next;
                            corrections.push(Correction { typed, instead });
                        }
                        Some(Suggested::DidYouMean(corrections))
                    }
                    // A state this build does not know is not a malformed
                    // answer — it is a newer node saying something in a
                    // vocabulary this client lacks, and the honest reading of
                    // that is the same as silence. The length prefix already
                    // carries the reader past whatever followed.
                    _ => None,
                }
            } else {
                None
            };
            Answer::Records {
                records,
                path,
                names,
                notes,
                only,
                exact,
                suggestion,
            }
        }
        tag::VALUE => {
            let (names, next) = take_names(body, at)?;
            let (bytes, next) = take_bytes(body, next)?;
            at = next;
            Answer::Value {
                value: decode_payload(&bytes)?,
                names,
            }
        }
        tag::KEYS => {
            let (count, next) = take_u32(body, at)?;
            at = next;
            let mut keys = Vec::new();
            for _ in 0..count {
                let (key, next) = take_text(body, at)?;
                at = next;
                keys.push(key);
            }
            Answer::Keys(keys)
        }
        tag::REMOVED => {
            let end = at.checked_add(8).ok_or(Error::Malformed)?;
            let bytes = body.get(at..end).ok_or(Error::Malformed)?;
            let mut held = [0_u8; 8];
            held.copy_from_slice(bytes);
            at = end;
            Answer::Removed(u64::from_be_bytes(held))
        }
        _ => Answer::Unknown,
    };
    // `at` has done its work inside this outcome; the caller advances by the
    // declared length instead, so a short read here cannot desynchronise the
    // stream.
    let _ = at;
    Ok(answer)
}

/// And back.
pub(crate) fn take_names(body: &[u8], at: usize) -> Result<(Names, usize)> {
    let (count, mut at) = take_u32(body, at)?;
    let mut names = Names::new();
    for _ in 0..count {
        let (table, next) = take_u32(body, at)?;
        let (name, next) = take_text(body, next)?;
        at = next;
        names.insert(TableId::new(table), name);
    }
    Ok((names, at))
}

/// The access path, as one byte.
#[cfg(feature = "server")]
pub(crate) const fn path_tag(path: AccessPath) -> u8 {
    match path {
        AccessPath::Record => 0,
        AccessPath::Index => 1,
        AccessPath::Scan => 2,
        AccessPath::Ordered => 3,
        AccessPath::Approximate => 4,
        AccessPath::Graph => 5,
        AccessPath::Join => 6,
        AccessPath::Materialised => 7,
        AccessPath::Span => 8,
    }
}

/// And back, as the name the store uses.
///
/// An unknown tag reads as the scan, which is the honest answer to a path this
/// build has no name for: it is the one path that promises nothing.
pub(crate) const fn path_name(tag: u8) -> &'static str {
    match tag {
        0 => "record",
        1 => "index",
        3 => "ordered",
        4 => "approximate",
        5 => "graph",
        6 => "join",
        7 => "materialised",
        8 => "span",
        _ => "scan",
    }
}

/// The identity of a record, as text.
///
/// Kept as the store spells it rather than re-parsed, because a client that
/// wants to name a record writes that text into its next script — and a second
/// reading of a record id is a second place for the two to disagree.
///
/// The spelling is the language's, which is what makes the sentence above true:
/// this returned `Display` until the wave that measured it, and `Display` writes
/// a UUID as thirty-two undivided digits and a text identity unquoted, neither
/// of which stands where the grammar puts an identity. A client following the
/// protocol to the letter got back text that its next script could not read.
///
/// Every place that puts an identity on this wire calls this. It returned the
/// wrong form partly because nothing called it at all — the encoder stringified
/// the id itself, so the one function documenting the promise was not the one
/// keeping it.
#[must_use]
pub fn spell(id: &RecordId) -> String {
    id.to_literal()
}
