//! `PATH TO … DEPTH n [WEIGHT f]` — the shortest path in a declared graph.
//!
//! # Why every assertion is against a brute force
//!
//! A shortest path that is merely short looks exactly like a shortest one, and a
//! hop-capped cheapest path found by a plain Dijkstra looks exactly like the
//! right one until the cheaper path needs one hop too many. So the store's answer
//! is compared, path for path, with every simple path within `n` hops enumerated
//! here and ordered by the same rule — fewest cost, then fewest steps, then the
//! smallest sequence of record ids — over generated graphs from fixed seeds.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId};

const NODES: i64 = 12;

/// A multiplicative generator, so a graph is a function of its seed.
struct Rolls(u64);

impl Rolls {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }
}

/// Weighted directed edges, `from → to → weight`, as halves so every sum is
/// exact.
type Edges = BTreeMap<i64, BTreeMap<i64, f64>>;

fn generated(seed: u64) -> Edges {
    let mut rolls = Rolls(seed.wrapping_add(0x5eed));
    let mut edges = Edges::new();
    for _ in 0..30 {
        let from = i64::try_from(rolls.below(12)).unwrap().saturating_add(1);
        let to = i64::try_from(rolls.below(12)).unwrap().saturating_add(1);
        if from == to {
            continue;
        }
        let halves = u32::try_from(rolls.below(10)).unwrap().saturating_add(1);
        edges
            .entry(from)
            .or_default()
            .insert(to, f64::from(halves) / 2.0);
    }
    edges
}

fn store_of(edges: &Edges) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    let mut script = String::from(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE town; USE DATABASE town;\n\
         DEFINE GRAPH roads; DEFINE TABLE place (n int) IN roads;\n\
         DEFINE EDGE road IN roads FROM place TO place;\n",
    );
    for n in 1..=NODES {
        script.push_str(&format!("CREATE place:{n} = {{ n: {n} }};\n"));
    }
    for (from, out) in edges {
        for (to, km) in out {
            script.push_str(&format!(
                "RELATE place:{from}->road->place:{to} = {{ km: {km} }};\n"
            ));
        }
    }
    session.run(&script).unwrap();
    store
}

/// The best path the brute force finds: every simple path from `from` to `to`
/// within `depth` steps, ordered by cost, then steps, then the ids.
fn brute(
    edges: &Edges,
    from: i64,
    to: i64,
    depth: usize,
    weighted: bool,
) -> Option<(Vec<i64>, f64)> {
    /// One search: the graph, the target, the cost rule and the best so far.
    struct Search<'a> {
        edges: &'a Edges,
        to: i64,
        weighted: bool,
        best: Option<(f64, usize, Vec<i64>)>,
    }

    impl Search<'_> {
        fn walk(&mut self, path: &mut Vec<i64>, left: usize, cost: f64) {
            let Some(at) = path.last().copied() else {
                return;
            };
            if at == self.to {
                let candidate = (cost, path.len().saturating_sub(1), path.clone());
                let better = self.best.as_ref().is_none_or(|held| {
                    (candidate.0, candidate.1, &candidate.2) < (held.0, held.1, &held.2)
                });
                if better {
                    self.best = Some(candidate);
                }
                return;
            }
            let Some(rest) = left.checked_sub(1) else {
                return;
            };
            let out: Vec<(i64, f64)> = self
                .edges
                .get(&at)
                .into_iter()
                .flatten()
                .map(|(next, km)| (*next, *km))
                .collect();
            for (next, km) in out {
                if path.contains(&next) {
                    continue;
                }
                path.push(next);
                let step = if self.weighted { km } else { 1.0 };
                self.walk(path, rest, cost + step);
                path.pop();
            }
        }
    }

    let mut search = Search {
        edges,
        to,
        weighted,
        best: None,
    };
    search.walk(&mut vec![from], depth, 0.0);
    search.best.map(|(cost, _, path)| (path, cost))
}

/// The store's path and the cost its note states.
fn asked(session: &mut Session<'_>, read: &str) -> (Vec<i64>, Option<(u64, f64)>) {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, notes, .. }) = outcomes.last() else {
        panic!("{read}: {:?}", outcomes.last());
    };
    let path = records
        .iter()
        .map(|(id, _)| match id {
            RecordId::Int(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect();
    let stated = notes.iter().find_map(|note| match note {
        Note::Path { steps, cost } => Some((
            *steps,
            match cost {
                Number::Integer(held) => f64::from(i32::try_from(*held).unwrap()),
                Number::Float(held) => *held,
                other => panic!("{other:?}"),
            },
        )),
        _ => None,
    });
    (path, stated)
}

#[test]
fn the_shortest_path_equals_the_brute_force_on_generated_graphs() {
    let mut compared = 0_usize;
    let mut found = 0_usize;
    for seed in 0..40_u64 {
        let edges = generated(seed);
        let store = store_of(&edges);
        let mut session = Session::new(&store);
        session
            .run("USE NAMESPACE prod; USE DATABASE town;")
            .unwrap();
        let mut rolls = Rolls(seed);
        for _ in 0..6 {
            let from = i64::try_from(rolls.below(12)).unwrap() + 1;
            let to = i64::try_from(rolls.below(12)).unwrap() + 1;
            let depth = usize::try_from(rolls.below(6)).unwrap() + 1;
            for weighted in [false, true] {
                let weight = if weighted { " WEIGHT km" } else { "" };
                let read = format!(
                    "SELECT * FROM place:{from}->road->place PATH TO place:{to} DEPTH {depth}{weight};"
                );
                let (path, stated) = asked(&mut session, &read);
                let expected = brute(&edges, from, to, depth, weighted);
                compared += 1;
                match expected {
                    None => {
                        assert!(path.is_empty(), "seed {seed}: {read} answered {path:?}");
                        assert_eq!(stated, None, "seed {seed}: {read}");
                    }
                    Some((best, cost)) => {
                        found += 1;
                        assert_eq!(path, best, "seed {seed}: {read}");
                        let steps = u64::try_from(best.len() - 1).unwrap();
                        assert_eq!(stated, Some((steps, cost)), "seed {seed}: {read}");
                    }
                }
            }
        }
    }
    // Enough of both outcomes that the comparison could have failed either way.
    assert!(
        found > 100 && compared - found > 40,
        "found {found} of {compared}"
    );
}

/// The cheapest path within the cap, not the cheapest path: 1 → 4 costs 1 over
/// three steps and 10 in one, and with `DEPTH 2` only the dear one is allowed.
/// A Dijkstra that ignored the cap would answer the three-step path.
#[test]
fn a_cheaper_path_with_too_many_steps_is_not_the_answer() {
    let mut edges = Edges::new();
    edges.entry(1).or_default().insert(4, 10.0);
    edges.entry(1).or_default().insert(2, 0.5);
    edges.entry(2).or_default().insert(3, 0.0);
    edges.entry(3).or_default().insert(4, 0.5);
    let store = store_of(&edges);
    let mut session = Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE town;")
        .unwrap();
    let read = |depth: u64| {
        format!("SELECT * FROM place:1->road->place PATH TO place:4 DEPTH {depth} WEIGHT km;")
    };
    assert_eq!(asked(&mut session, &read(2)), (vec![1, 4], Some((1, 10.0))));
    assert_eq!(
        asked(&mut session, &read(3)),
        (vec![1, 2, 3, 4], Some((3, 1.0)))
    );
    // The start is its own path, and nothing within reach answers nothing.
    assert_eq!(
        asked(
            &mut session,
            "SELECT * FROM place:1->road->place PATH TO place:1 DEPTH 3;"
        ),
        (vec![1], Some((0, 0.0)))
    );
    assert_eq!(
        asked(
            &mut session,
            "SELECT * FROM place:4->road->place PATH TO place:1 DEPTH 3;"
        ),
        (Vec::new(), None)
    );
}

/// The work is the reachable subgraph, each node expanded once, however large the
/// number is written: a complete graph walked to `DEPTH 1000000`.
#[test]
fn a_large_depth_over_a_dense_graph_costs_the_graph() {
    let mut edges = Edges::new();
    for from in 1..=NODES {
        for to in 1..=NODES {
            if from != to {
                edges.entry(from).or_default().insert(to, 1.0);
            }
        }
    }
    let store = store_of(&edges);
    let mut session = Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE town;")
        .unwrap();
    let started = std::time::Instant::now();
    for weight in ["", " WEIGHT km"] {
        let read =
            format!("SELECT * FROM place:1->road->place PATH TO place:12 DEPTH 1000000{weight};");
        assert_eq!(asked(&mut session, &read), (vec![1, 12], Some((1, 1.0))));
    }
    // An end nothing reaches: the walk stops when the reachable set is spent,
    // not when the number written runs out.
    session.run("CREATE place:13 = { n: 13 };").unwrap();
    for weight in ["", " WEIGHT km"] {
        let read =
            format!("SELECT * FROM place:1->road->place PATH TO place:13 DEPTH 1000000{weight};");
        assert_eq!(asked(&mut session, &read), (Vec::new(), None));
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn an_unbounded_or_unweighable_path_is_refused_by_name() {
    let mut edges = Edges::new();
    edges.entry(1).or_default().insert(2, 1.0);
    let store = store_of(&edges);
    let mut session = Session::new(&store);
    session
        .run("USE NAMESPACE prod; USE DATABASE town;")
        .unwrap();
    let error = session
        .run("SELECT * FROM place:1->road->place PATH TO place:2;")
        .unwrap_err();
    assert!(error.to_string().contains("DEPTH"), "{error}");
    assert!(
        matches!(
            &error,
            Error::Script(tessari_ql::Error::PathNeedsDepth { .. })
        ),
        "{error:?}"
    );
    session
        .run("RELATE place:6->road->place:7 = { km: 'far' }; RELATE place:8->road->place:9 = { km: -1 };")
        .unwrap();
    for (from, to) in [(6, 7), (8, 9)] {
        let error = session
            .run(&format!(
                "SELECT * FROM place:{from}->road->place PATH TO place:{to} DEPTH 3 WEIGHT km;"
            ))
            .unwrap_err();
        assert!(matches!(error, Error::PathWeight { .. }), "{error:?}");
    }
    // An edge with no weight is no road at all for a weighted path.
    session.run("RELATE place:1->road->place:5;").unwrap();
    assert_eq!(
        asked(
            &mut session,
            "SELECT * FROM place:1->road->place PATH TO place:5 DEPTH 3 WEIGHT km;"
        ),
        (Vec::new(), None)
    );
    assert_eq!(
        asked(
            &mut session,
            "SELECT * FROM place:1->road->place PATH TO place:5 DEPTH 3;"
        ),
        (vec![1, 5], Some((1, 1.0)))
    );
}
