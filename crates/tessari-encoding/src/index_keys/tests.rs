#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_types::{DatabaseId, IndexId, NamespaceId, TableId};

use tessari_types::Value;

use super::{
    IndexAddress, IndexValues, SearchStatistics, SearchStatisticsKey, StoreKey, StoreValue,
};

fn address() -> IndexAddress {
    IndexAddress::new(
        NamespaceId::new(3),
        DatabaseId::new(4),
        TableId::new(5),
        IndexId::new(6),
    )
}

#[test]
fn one_index_has_exactly_one_statistics_key() {
    // No suffix, so the key *is* the index prefix — which is what makes
    // reading it a point read rather than a scan for the one entry.
    let key = SearchStatisticsKey::new(address());
    let encoded = key.encode();
    assert_eq!(encoded.as_slice().len(), super::INDEX_PREFIX_LEN);
    let read = SearchStatisticsKey::decode(encoded.as_slice()).expect("a key");
    assert_eq!(read, key);
}

#[test]
fn both_counts_survive_the_round_trip() {
    let held = SearchStatistics::new(1_234, 98_765);
    let encoded = held.encode();
    let read = SearchStatistics::decode(encoded.as_slice()).expect("statistics");
    assert_eq!(read, held);
}

#[test]
fn a_dictionary_entry_survives_the_round_trip_and_carries_no_record() {
    use super::SearchTermKey;
    let term = IndexValues::of(&[Value::from("vector")]);
    let key = SearchTermKey::new(address(), term.clone());
    let encoded = key.encode();
    let read = SearchTermKey::decode(encoded.as_slice()).expect("a key");
    assert_eq!(read, key);
    // One entry per term, so the key ends where the term ends. A record
    // suffix would make the dictionary as long as the posting list and
    // remove the whole reason for it.
    assert_eq!(
        encoded.as_slice().len(),
        super::INDEX_PREFIX_LEN + term.as_slice().len()
    );
}

#[test]
fn a_member_posting_keeps_each_fields_frequency_and_length() {
    use super::{Located, Posting, StoreValue};
    let located = Located {
        fields: vec![(2, 5), (0, 9), (1, 40)],
        ..Located::default()
    };
    let encoded = Posting::encode_located(3, 54, &located);
    assert_eq!(Posting::located(encoded.as_slice()).unwrap(), located);
    // Still the counted posting every reader knows.
    assert_eq!(
        Posting::decode(encoded.as_slice()).unwrap(),
        Posting::Counted {
            frequency: 3,
            length: 54
        }
    );
    // With no fields it is byte-identical to the counted form.
    assert_eq!(
        Posting::encode_located(3, 54, &Located::default()).as_slice(),
        Posting::Counted {
            frequency: 3,
            length: 54
        }
        .encode()
        .as_slice()
    );
}

#[test]
fn a_surface_pair_and_its_count_survive_the_round_trip_and_a_walk_bounds_them() {
    use super::SearchSurfaceKey;
    let key = SearchSurfaceKey::new(address(), "transactions".to_owned(), "transact".to_owned());
    let read = SearchSurfaceKey::decode(key.encode().as_slice()).expect("a key");
    assert_eq!(read, key);
    let counted = SearchSurfaceKey::count(7);
    assert_eq!(
        SearchSurfaceKey::counted(counted.as_slice()).expect("a count"),
        7
    );
    // The leading letters bound exactly the surfaces that begin with them.
    let bounds = SearchSurfaceKey::surface_prefix(&address(), "tr");
    assert!(key.encode().as_slice().starts_with(&bounds));
    let other = SearchSurfaceKey::new(address(), "replicas".to_owned(), "replica".to_owned());
    assert!(!other.encode().as_slice().starts_with(&bounds));
}

#[test]
fn a_dictionary_entry_and_its_postings_spell_the_term_the_same_way() {
    use super::{PostingKey, SearchTermKey};
    // The load-bearing property: a prefix walk of the dictionary finds the
    // terms whose postings a lookup then reads. Two encodings would be two
    // vocabularies, and the walk would reach terms the lookup could not.
    let term = IndexValues::of(&[Value::from("vector")]);
    let dictionary = SearchTermKey::new(address(), term.clone()).encode();
    let postings = PostingKey::term_prefix(&address(), &term);
    assert_eq!(
        &dictionary.as_slice()[super::INDEX_PREFIX_LEN..],
        &postings[super::INDEX_PREFIX_LEN..]
    );
    // And only the kind byte differs, so scanning one never reaches the
    // other.
    assert_ne!(dictionary.as_slice()[0], postings[0]);
}

#[test]
fn a_term_prefix_bounds_exactly_the_terms_that_begin_with_it() {
    use super::SearchTermKey;
    let bounds = SearchTermKey::term_prefix(&address(), "vect");
    let under = ["vect", "vector", "vectorised"];
    for term in under {
        let key = SearchTermKey::new(address(), IndexValues::of(&[Value::from(term)])).encode();
        assert!(key.as_slice().starts_with(&bounds), "{term} not under vect");
    }
    // And nothing else is, including the words a naive substring match
    // would admit — the escape is byte-local, so no filtering step is owed.
    for term in ["vec", "invective", "wave"] {
        let key = SearchTermKey::new(address(), IndexValues::of(&[Value::from(term)])).encode();
        assert!(!key.as_slice().starts_with(&bounds), "{term} under vect");
    }
}

#[test]
fn a_term_frequency_survives_the_round_trip() {
    use super::TermStatistics;
    for documents in [0_u64, 1, 12_345, u64::MAX] {
        let held = TermStatistics::new(documents);
        let read = TermStatistics::decode(held.encode().as_slice()).expect("statistics");
        assert_eq!(read, held, "{documents}");
    }
}

#[test]
fn a_dictionary_entry_from_a_later_format_is_refused_rather_than_half_read() {
    use super::TermStatistics;
    // `max_impact` will arrive as a longer payload. Reading such an entry as
    // though it were this build's would report a frequency out of a format
    // whose meaning this build cannot check — the same refusal a posting
    // with a trailing byte already gets.
    let mut bytes = TermStatistics::new(7).encode().as_slice().to_vec();
    bytes.push(0);
    assert!(
        TermStatistics::decode(&bytes).is_err(),
        "trailing byte read"
    );
}

#[test]
fn a_counted_posting_survives_the_round_trip() {
    use super::Posting;
    for (frequency, length) in [(1_u32, 1_u32), (3, 97), (u32::MAX, u32::MAX), (1, u32::MAX)] {
        let held = Posting::Counted { frequency, length };
        let read = Posting::decode(held.encode().as_slice()).expect("a posting");
        assert_eq!(read, held, "{frequency}/{length}");
    }
}

#[test]
fn a_posting_with_no_payload_is_the_membership_one_an_older_format_wrote() {
    use super::{NoPayload, Posting};
    // Byte-identical, because it is the same statement — which is what lets
    // an index written before postings carried a payload keep answering
    // `MATCHES` instead of failing to decode.
    assert_eq!(
        Posting::Membership.encode().as_slice(),
        NoPayload.encode().as_slice()
    );
    let read = Posting::decode(NoPayload.encode().as_slice()).expect("a posting");
    assert_eq!(read, Posting::Membership);
}

#[test]
fn a_counted_posting_is_not_mistaken_for_a_membership_one() {
    use super::Posting;
    // The distinction is carried by the encoding rather than by a declared
    // version, so it cannot disagree with the data. A zero frequency is
    // still `Counted`: it is a statement, where `Membership` is the absence
    // of one.
    let zero = Posting::Counted {
        frequency: 0,
        length: 0,
    };
    assert_ne!(
        zero.encode().as_slice(),
        Posting::Membership.encode().as_slice()
    );
    assert_eq!(
        Posting::decode(zero.encode().as_slice()).expect("a posting"),
        zero
    );
}

#[test]
fn a_truncated_posting_payload_is_refused_rather_than_read_short() {
    use super::Posting;
    let full = Posting::Counted {
        frequency: 7,
        length: 11,
    };
    let encoded = full.encode();
    let bytes = encoded.as_slice();
    // Every cut between the header and the end: a decoder that read a short
    // payload as a smaller number would return a plausible wrong score.
    for cut in 3..bytes.len() {
        assert!(
            Posting::decode(&bytes[..cut]).is_err(),
            "{cut} bytes decoded when it should not"
        );
    }
}

#[test]
fn a_posting_payload_longer_than_the_format_is_refused() {
    use super::Posting;
    // Found by falsification: dropping `reader.finish()` left every other
    // assertion green, because a short payload is caught by the reads
    // themselves and nothing here asked about a long one. Trailing bytes
    // mean the value was written by something this build does not
    // understand, and reading the prefix of it would be reading two numbers
    // out of a structure that has more.
    let mut bytes = Posting::Counted {
        frequency: 7,
        length: 11,
    }
    .encode()
    .as_slice()
    .to_vec();
    bytes.push(0);
    assert!(Posting::decode(&bytes).is_err(), "trailing byte accepted");
}

#[test]
fn a_vector_node_survives_the_round_trip() {
    use super::{QuantizedVector, RecordId, StoredVector, VectorNode, VectorNodeKey};
    let components = vec![0.123, 0.999, 0.0, 1.0, 0.5];
    let node = VectorNode::new(
        StoredVector::Full(components.clone()),
        vec![RecordId::Int(1), RecordId::Int(2)],
    );
    let encoded = node.encode();
    let read = VectorNode::decode(encoded.as_slice()).expect("a node");
    assert_eq!(read, node);

    let coded = QuantizedVector::of(&components).expect("codes");
    let quantized = VectorNode::new(
        StoredVector::Quantized(coded),
        vec![RecordId::Int(1), RecordId::Int(2)],
    );
    let small = quantized.encode();
    assert_eq!(
        VectorNode::decode(small.as_slice()).expect("a node"),
        quantized
    );
    // The layout `stored_bytes` states: the two nodes share their neighbour
    // lists, so their sizes differ by exactly the two vectors' forms.
    assert_eq!(
        encoded.as_slice().len() - small.as_slice().len(),
        node.vector.stored_bytes() - quantized.vector.stored_bytes()
    );

    let key = VectorNodeKey::new(address(), 0, RecordId::Int(7));
    let bytes = key.encode();
    assert_eq!(VectorNodeKey::decode(bytes.as_slice()).expect("a key"), key);
}

#[test]
fn a_full_precision_node_keeps_the_bytes_it_always_had() {
    // Every vector index written before `QUANTIZED` holds these bytes, and
    // a node is never rewritten by an upgrade — so the full form's encoding
    // is pinned, not merely round-tripped.
    use super::{RecordId, StoredVector, VectorNode};
    let node = VectorNode::new(StoredVector::Full(vec![0.5, -1.0]), vec![RecordId::Int(3)]);
    assert_eq!(node.encode().as_slice(), GOLDEN_FULL_NODE);
}

const GOLDEN_FULL_NODE: &[u8] = &[
    1, 0, 0, 0, 0, 2, 63, 224, 0, 0, 0, 0, 0, 0, 191, 240, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 128, 0,
    0, 0, 0, 0, 0, 3,
];

#[test]
fn an_empty_index_has_no_average_length() {
    // Not zero: a score divides by this, and dividing by a number that is
    // not one is worse than being told there is no number.
    assert_eq!(SearchStatistics::default().average_length(), None);
    assert_eq!(SearchStatistics::new(0, 0).average_length(), None);
}

#[test]
fn the_average_is_tokens_over_documents() {
    let held = SearchStatistics::new(4, 30);
    assert_eq!(held.average_length(), Some(7.5));
}

#[test]
fn a_count_converts_without_a_cast_and_without_saturating() {
    // The split-halves conversion has to reach the same number a cast would,
    // including past `u32`, or a large collection would be ranked against a
    // length that is not its own.
    let held = SearchStatistics::new(1, u64::from(u32::MAX) + 1);
    assert_eq!(held.average_length(), Some(4_294_967_296.0));
    let bigger = SearchStatistics::new(2, 1 << 40);
    assert_eq!(bigger.average_length(), Some(549_755_813_888.0));
}

#[test]
fn the_leading_values_of_a_composite_entry_are_the_bytes_a_shorter_entry_encodes() {
    // What an order over the leading field of a composite index compares.
    // Whether two entries share a `last` is asked of the *bytes*, because
    // the encoding cannot be reversed — so the bytes have to be exactly what
    // a one-value entry encodes, or a tie group would be recognised by a
    // rule the writer does not follow.
    let composite = IndexValues::of(&[Value::from("ward"), Value::from("ada")]);
    let other_first = IndexValues::of(&[Value::from("ward"), Value::from("zoe")]);
    let other_last = IndexValues::of(&[Value::from("wardle"), Value::from("ada")]);

    let one = composite.leading_of(1).unwrap();
    assert_eq!(one, IndexValues::leading(&[Value::from("ward")]).as_slice());
    assert_eq!(one, other_first.leading_of(1).unwrap());
    assert_ne!(one, other_last.leading_of(1).unwrap());

    // `ward` must not be the leading run of `wardle`, or the tie group would
    // swallow the next value's entries. This is the self-delimiting property
    // stated as a test rather than as a comment.
    assert!(!other_last.leading_of(1).unwrap().starts_with(one));
}

#[test]
fn asking_for_more_fields_than_the_entry_holds_compares_all_of_them() {
    // The failure this refuses is silent: comparing *fewer* values than
    // asked for would merge tie groups that are not tied, and the answer
    // would be short with every record it returned real.
    let entry = IndexValues::of(&[Value::from("ward"), Value::from("ada")]);
    let all = entry.leading_of(2).unwrap();
    assert_eq!(all, entry.leading_of(9).unwrap());
    assert_eq!(
        all,
        IndexValues::leading(&[Value::from("ward"), Value::from("ada")]).as_slice()
    );
    assert_eq!(entry.leading_of(0).unwrap(), b"");
}

#[test]
fn two_spellings_of_one_number_lead_with_the_same_bytes() {
    // The normalisation that makes the encoding one-way is what makes byte
    // equality the *right* tie test: `1` and `1.0` are one value, so they
    // belong in one tie group and must not be walked as two.
    let integer = IndexValues::of(&[Value::from(1_i64), Value::from("a")]);
    let float = IndexValues::of(&[Value::from(1.0_f64), Value::from("b")]);
    assert_eq!(integer.leading_of(1).unwrap(), float.leading_of(1).unwrap());
}

/// Every shape a posting's lists take, written and read back, and the plain
/// counted posting unchanged by the option machinery (G051 T7.6).
#[test]
fn a_posting_carries_its_lists_and_reads_them_back() {
    use super::{Located, Posting};
    let plain = Posting::Counted {
        frequency: 2,
        length: 9,
    }
    .encode();
    assert_eq!(
        Posting::encode_located(2, 9, &Located::default()),
        plain,
        "an index with no option writes exactly what it always wrote"
    );
    for located in [
        Located {
            positions: vec![1, 7],
            ..Located::default()
        },
        Located {
            offsets: vec![(0, 3), (40, 44)],
            ..Located::default()
        },
        Located {
            positions: vec![1, 7],
            offsets: vec![(0, 3), (40, 44)],
            ..Located::default()
        },
    ] {
        let stored = Posting::encode_located(2, 9, &located);
        assert_eq!(Posting::located(stored.as_slice()).unwrap(), located);
        assert_eq!(
            Posting::decode(stored.as_slice()).unwrap(),
            Posting::Counted {
                frequency: 2,
                length: 9
            }
        );
    }
    assert_eq!(
        Posting::located(plain.as_slice()).unwrap(),
        Located::default()
    );
}

#[test]
fn a_posting_whose_lists_do_not_add_up_is_refused() {
    use super::{Located, Posting};
    let stored = Posting::encode_located(
        2,
        9,
        &Located {
            positions: vec![1, 7],
            ..Located::default()
        },
    );
    let bytes = stored.as_slice();
    // One position short.
    let short = &bytes[..bytes.len() - 4];
    assert!(Posting::located(short).is_err());
    assert!(Posting::decode(short).is_err());
    // One byte too many.
    let mut long = bytes.to_vec();
    long.push(0);
    assert!(Posting::located(&long).is_err());
    // A flag this build does not know (4 names the fields list).
    let mut unknown = bytes.to_vec();
    let flags_at = unknown.len() - 9;
    unknown[flags_at] |= 8;
    assert!(matches!(
        Posting::located(&unknown),
        Err(crate::Error::ReservedFlags { .. })
    ));
}
