//! What a read noticed about the values it compared.
//!
//! # The failure this is for
//!
//! A schemaless store lets a field hold a number in one record and the text of a
//! number in the next, and `WHERE age = 30` then matches some of them. Nothing
//! goes wrong: the comparison is well defined, the answer is correct for the
//! values that are actually there, and the read returns fewer records than the
//! author expected with no error anywhere. It is the same shape as the index
//! that quietly stops being used — correct, quiet, and wrong about the question
//! that was asked.
//!
//! So the read says what it compared, once, as a note.
//!
//! # An absence is never a mismatch
//!
//! A record without the field compares `none` against whatever the other side
//! holds, and that is the **ordinary** case a schemaless read is built for: it is
//! how a read over records of differing shapes narrows instead of failing. A note
//! on it would fire on nearly every read in the language, which is the mistake
//! slice U1 made once already and caught — a note keyed on something ordinary is
//! worse than no note, because it looks like a feature.
//!
//! `null` is left out for the same reason from the other direction: it is a value
//! that says nothing, deliberately written, and comparing it is not a mistake.
//!
//! # Why the sink hangs off the scope
//!
//! [`Scope`] is what the evaluator can see, it is passed to every evaluation, and
//! it holds only borrows — so a sink borrowed by it **cannot outlive the read**.
//! That is the property slice U1 wanted when it refused to keep the note buffer
//! on the `Session`: a buffer there outlives the statement that filled it, and a
//! note reported against the next answer is worse than no note. Here the lifetime
//! makes the guarantee instead of the discipline.
//!
//! [`Scope`]: crate::evaluate::Scope

use std::cell::RefCell;
use std::collections::BTreeSet;

use tessari_types::Value;

use crate::outcome::Note;

/// The kind pairs one read compared across.
///
/// A set rather than a list: a comparison runs once per record, so a read over a
/// million records that mixes two kinds has one thing to say and not a million.
#[derive(Debug, Default)]
pub(crate) struct Noticed {
    /// Ordered so the same mismatch reads the same way whichever side it was
    /// written on, and deduplicated so the count is of *kinds* and not records.
    pairs: RefCell<BTreeSet<(&'static str, &'static str)>>,
}

impl Noticed {
    /// Record that these two values were compared, if their kinds differ.
    ///
    /// Cheap and silent for the overwhelmingly common case: two values of one
    /// kind, or a comparison against an absence, take one comparison of two
    /// `&'static str` pointers and touch nothing.
    pub(crate) fn compared(&self, left: &Value, right: &Value) {
        // An absence is how a schemaless read narrows, not a mistake. See the
        // module note — this is the check that keeps the note rare enough to be
        // worth reading.
        if matches!(left, Value::None | Value::Null) || matches!(right, Value::None | Value::Null) {
            return;
        }
        let (left, right) = (left.type_name(), right.type_name());
        if left == right {
            return;
        }
        // A borrow that is only ever taken here and in `into_notes`, neither of
        // which can run while the other holds it: evaluation is single-threaded
        // and the read is finished before its notes are drained.
        if let Ok(mut pairs) = self.pairs.try_borrow_mut() {
            pairs.insert(if left <= right {
                (left, right)
            } else {
                (right, left)
            });
        }
    }

    /// The notes this read earned, one per pair of kinds it compared across.
    ///
    /// Taken by shared reference and **draining**, because the consumers that
    /// write to it are still alive at the point the read reports: a by-value
    /// drain would ask the read to prove it had dropped every borrow, which
    /// costs a restructuring to buy nothing. Draining also makes a second call
    /// answer nothing, which is the right answer to asking twice.
    pub(crate) fn drained(&self) -> Vec<Note> {
        let Ok(mut pairs) = self.pairs.try_borrow_mut() else {
            return Vec::new();
        };
        std::mem::take(&mut *pairs)
            .into_iter()
            .map(|(left, right)| Note::ComparedAcrossKinds { left, right })
            .collect()
    }
}
