//! A score with its parts: `search::explain` (ADR-0100 D1.8).
//!
//! # The same arithmetic, in the same order
//!
//! An explanation is worth reading only if its total **is** the score, and two
//! floating-point sums agree to the last bit only when they add the same
//! numbers in the same order. So this walks exactly what [`super::scored`]
//! walks — every asked word in the order written, repeats included, then every
//! starred word — under the same conditions, and a part the record does not
//! hold adds an exact `0.0`. The suite asserts the equality with `==`, not with
//! a tolerance; a tolerance would hide the day the two drift apart.

use std::collections::BTreeMap;

use tessari_types::{Number, Value};

use super::{Corpus, Held, as_float, inverse_document_frequency, positive, saturation};

/// The score this record earns against the corpus, with what each part of the
/// query contributed to it.
///
/// `weight` is the part's rarity across the collection, `held` how often the
/// record holds it, and `contribution` what it added. A word the record does
/// not hold is listed with `held: 0` and contributes nothing, so a reader can
/// see that it was asked and why it did not count.
pub(crate) fn explain(corpus: &Corpus, held: &Held) -> Value {
    let length = as_float(u64::from(held.length));
    let total = as_float(corpus.documents);
    // `scored`'s early answers, as one condition: nothing to weigh, or nothing
    // to weigh it against.
    let average = if (corpus.asked.is_empty() && corpus.blends.is_empty()) || corpus.documents == 0
    {
        None
    } else {
        positive(corpus.average_length)
    };
    let weigh = |documents: u64, occurrences: u64| {
        let weight = inverse_document_frequency(total, as_float(documents));
        let contribution = match average {
            Some(average) if occurrences > 0 => {
                weight * saturation(as_float(occurrences), length, average)
            }
            _ => 0.0,
        };
        (weight, contribution)
    };

    let mut sum = 0.0_f64;
    let mut terms = Vec::with_capacity(corpus.asked.len());
    for term in &corpus.asked {
        let occurrences = u64::from(held.occurrences.get(term).copied().unwrap_or(0));
        let documents = corpus.terms.get(term).map_or(0, |entry| entry.documents);
        let (weight, contribution) = weigh(documents, occurrences);
        sum += contribution;
        terms.push(object([
            ("term", Value::String(term.clone())),
            ("held", integer(occurrences)),
            ("documents", integer(documents)),
            ("weight", float(weight)),
            ("contribution", float(contribution)),
        ]));
    }

    let mut prefixes = Vec::with_capacity(corpus.blends.len());
    for blend in &corpus.blends {
        let found: Vec<(&String, u32)> = blend
            .expansions
            .iter()
            .filter_map(|term| held.occurrences.get(term).map(|times| (term, *times)))
            .collect();
        let occurrences = found.iter().fold(0_u64, |count, (_, times)| {
            count.saturating_add(u64::from(*times))
        });
        let (weight, contribution) = weigh(blend.documents, occurrences);
        sum += contribution;
        prefixes.push(object([
            ("prefix", Value::String(blend.prefix.clone())),
            (
                "terms",
                Value::Array(
                    found
                        .iter()
                        .map(|(term, _)| Value::String((*term).clone()))
                        .collect(),
                ),
            ),
            ("held", integer(occurrences)),
            ("documents", integer(blend.documents)),
            ("weight", float(weight)),
            ("contribution", float(contribution)),
        ]));
    }

    object([
        ("score", float(sum)),
        ("documents", integer(corpus.documents)),
        ("average_length", float(corpus.average_length)),
        ("length", integer(u64::from(held.length))),
        ("terms", Value::Array(terms)),
        ("prefixes", Value::Array(prefixes)),
    ])
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn integer(count: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn float(value: f64) -> Value {
    Value::Number(Number::float(value))
}
