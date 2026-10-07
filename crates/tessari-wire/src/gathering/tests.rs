#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn a_gather_and_a_page_round_trip_and_a_cut_body_is_refused() {
    let asked = Gather {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        shard: ShardId::new(4),
        from: Some(RecordId::Text("g".to_owned())),
        to: Some((RecordId::Int(-7), true)),
        after: Some(RecordId::Uuid([9; 16])),
        pushed: None,
        enough: None,
        reduce: None,
        ordered: None,
        counting: None,
    };
    let body = asked.encode();
    assert_eq!(Gather::decode(&body).unwrap(), asked);
    assert!(matches!(
        Gather::decode(body.get(..body.len() - 1).unwrap()),
        Err(Error::Malformed)
    ));
    let open = Gather {
        from: None,
        to: None,
        after: None,
        ..asked
    };
    assert_eq!(Gather::decode(&open.encode()).unwrap(), open);
    let narrowed = Gather {
        pushed: Some(tessari_session::Pushed {
            visible: Some(["total".to_owned()].into()),
            condition: "(total > $p0)".to_owned(),
            parameters: [("p0".to_owned(), tessari_types::Value::from(3_i64))].into(),
        }),
        ..open.clone()
    };
    assert_eq!(Gather::decode(&narrowed.encode()).unwrap(), narrowed);
    let bounded = Gather {
        enough: Some(3),
        ..narrowed.clone()
    };
    assert_eq!(Gather::decode(&bounded.encode()).unwrap(), bounded);
    let only_bounded = Gather {
        pushed: None,
        ..bounded
    };
    assert_eq!(
        Gather::decode(&only_bounded.encode()).unwrap(),
        only_bounded
    );
    let folding = Gather {
        enough: Some(3),
        reduce: Some(folds(Some("(total > $p0)"))),
        ..narrowed.clone()
    };
    assert_eq!(Gather::decode(&folding.encode()).unwrap(), folding);
    let only_folding = Gather {
        reduce: Some(folds(None)),
        ..open.clone()
    };
    assert_eq!(
        Gather::decode(&only_folding.encode()).unwrap(),
        only_folding
    );
    let ranked = Gather {
        ordered: Some(by_n(true, 2)),
        ..narrowed.clone()
    };
    assert_eq!(Gather::decode(&ranked.encode()).unwrap(), ranked);
    // A direction that is neither is a frame read wrongly, not a third one.
    let mut sideways = ranked.encode();
    let at = sideways.len() - 9;
    assert_eq!(sideways.get(at), Some(&1));
    *sideways.get_mut(at).unwrap() = 7;
    assert!(matches!(Gather::decode(&sideways), Err(Error::Malformed)));

    let page = Page {
        records: vec![
            (RecordId::Bytes(vec![0, 1]), vec![5, 6, 7]),
            (RecordId::Text("h".to_owned()), Vec::new()),
        ],
        more: true,
        resume: None,
        reduced: None,
        counted: None,
    };
    let body = page.encode();
    assert_eq!(Page::decode(&body).unwrap(), page);
    let mut long = body.clone();
    long.push(0);
    assert!(matches!(Page::decode(&long), Err(Error::Malformed)));
    let resuming = Page {
        resume: Some(RecordId::Text("z".to_owned())),
        ..page
    };
    assert_eq!(Page::decode(&resuming.encode()).unwrap(), resuming);
    let folded = Page {
        records: Vec::new(),
        reduced: Some(tessari_session::Reduced::Partials(vec![
            tessari_session::Partial {
                key: vec![tessari_types::Value::from("x"), tessari_types::Value::None],
                first: RecordId::Text("a".to_owned()),
                states: vec![tessari_types::Value::from(2_i64)],
            },
        ])),
        ..resuming.clone()
    };
    assert_eq!(Page::decode(&folded.encode()).unwrap(), folded);
    let declined = Page {
        resume: None,
        reduced: Some(tessari_session::Reduced::Declined),
        ..folded
    };
    assert_eq!(Page::decode(&declined.encode()).unwrap(), declined);
    // A fold this build does not merge exactly is not read as another one.
    let mut unknown = only_folding.encode();
    let at = unknown
        .windows(5)
        .position(|held| held == b"count")
        .unwrap();
    unknown.splice(at..at + 5, *b"blurt");
    assert!(matches!(Gather::decode(&unknown), Err(Error::Malformed)));
}

/// Ranked by `n`, keeping `most`.
pub(super) fn by_n(descending: bool, most: u64) -> tessari_session::Ordered {
    tessari_session::Ordered {
        visible: None,
        keys: vec![tessari_session::OrderKey {
            key: "n".to_owned(),
            parameters: tessari_session::Parameters::new(),
            descending,
        }],
        most,
    }
}

/// `count(*)` and `sum(total)`, under an optional condition over `$p0`.
fn folds(condition: Option<&str>) -> tessari_session::Reduce {
    let text = |text: &str| (text.to_owned(), tessari_session::Parameters::new());
    tessari_session::Reduce {
        visible: Some(["total".to_owned()].into()),
        condition: condition.map(|condition| {
            (
                condition.to_owned(),
                [("p0".to_owned(), tessari_types::Value::from(3_i64))].into(),
            )
        }),
        keys: vec![text("note")],
        folds: vec![
            tessari_session::Folded::named("count", None, None).unwrap(),
            tessari_session::Folded::named("sum", Some(text("total")), None).unwrap(),
        ],
        samples: false,
    }
}

/// ADR-0121 D6 — the holding folds round-trip, a counter fold with its instant
/// and the samples byte; a read with no counter fold encodes as it always did,
/// so what an older leader is sent does not depend on the new flag.
#[test]
fn the_holding_folds_round_trip_and_only_a_counter_fold_carries_the_samples_byte() {
    let text = |text: &str| Some((text.to_owned(), tessari_session::Parameters::new()));
    let reduce = |samples: bool| tessari_session::Reduce {
        visible: None,
        condition: None,
        keys: Vec::new(),
        folds: vec![
            tessari_session::Folded::named("median", text("n"), None).unwrap(),
            tessari_session::Folded::named("collect", text("n"), None).unwrap(),
            tessari_session::Folded::named("increase", text("n"), text("at")).unwrap(),
        ],
        samples,
    };
    let asking = |reduce: tessari_session::Reduce| Gather {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        shard: ShardId::new(4),
        from: None,
        to: None,
        after: None,
        pushed: None,
        enough: None,
        reduce: Some(reduce),
        ordered: None,
        counting: None,
    };
    for samples in [false, true] {
        let asked = asking(reduce(samples));
        assert_eq!(Gather::decode(&asked.encode()).unwrap(), asked);
    }
    let summary = asking(reduce(false)).encode();
    let samples = asking(reduce(true)).encode();
    let differ: Vec<usize> = (0..summary.len())
        .filter(|at| summary.get(*at) != samples.get(*at))
        .collect();
    assert_eq!(differ.len(), 1, "one byte says which");
    let mut third = samples;
    *third.get_mut(*differ.first().unwrap()).unwrap() = 2;
    assert!(matches!(Gather::decode(&third), Err(Error::Malformed)));
    // No counter fold: the flag has nowhere to go.
    let constant = |samples| tessari_session::Reduce {
        samples,
        ..folds(None)
    };
    assert_eq!(
        asking(constant(false)).encode(),
        asking(constant(true)).encode()
    );
    // A counter fold without its instant is not a request this build reads.
    assert!(tessari_session::Folded::named("increase", text("n"), None).is_none());
    assert!(tessari_session::Folded::named("median", text("n"), text("at")).is_none());
}

#[test]
fn a_count_and_its_answer_round_trip_and_a_term_the_body_lacks_is_refused() {
    let asked = Gather {
        namespace: NamespaceId::new(1),
        database: DatabaseId::new(2),
        table: TableId::new(3),
        shard: ShardId::new(4),
        from: None,
        to: None,
        after: Some(RecordId::from("b")),
        pushed: None,
        enough: None,
        reduce: None,
        ordered: None,
        counting: Some(tessari_session::Counting {
            index: tessari_types::IndexId::new(7),
            terms: ["fox".to_owned(), "dog".to_owned()].to_vec(),
        }),
    };
    let body = asked.encode();
    assert_eq!(Gather::decode(&body).unwrap(), asked);
    assert!(matches!(
        Gather::decode(body.get(..body.len() - 1).unwrap()),
        Err(Error::Malformed)
    ));
    let page = Page {
        records: Vec::new(),
        more: true,
        resume: Some(RecordId::from("c")),
        reduced: None,
        counted: Some(tessari_storage::SearchCounts {
            documents: 3,
            tokens: 8,
            holding: vec![2, 0],
        }),
    };
    let body = page.encode();
    assert_eq!(Page::decode(&body).unwrap(), page);
    // A count claiming a third term the body does not carry.
    let mut claimed = body.clone();
    let at = claimed.len() - 2 * 8 - 4;
    *claimed.get_mut(at + 3).unwrap() = 3;
    assert!(matches!(Page::decode(&claimed), Err(Error::Malformed)));
}

#[test]
fn every_reason_keeps_its_byte_and_an_unknown_one_is_refused() {
    for reason in [
        Ungathered::NotEntitled,
        Ungathered::NotHeld,
        Ungathered::NoSuchTable,
        Ungathered::MapMoved,
    ] {
        assert_eq!(Ungathered::from_byte(reason.byte()).unwrap(), reason);
    }
    assert!(matches!(Ungathered::from_byte(0), Err(Error::Malformed)));
    assert!(matches!(Ungathered::from_byte(5), Err(Error::Malformed)));
}
