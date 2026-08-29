//! Two evaluators of one rule set, asked the same questions.
//!
//! # Why a second implementation at all
//!
//! Because a test suite written beside an implementation asks it what it does
//! and then checks it did that. It catches a regression and it cannot catch a
//! rule that was transcribed wrongly in the first place — and this store's
//! authority model was transcribed from a document into a match arm fifty times.
//!
//! So the second evaluator here is written **from the decision-semantics
//! document**, not from `Needs::of`. Where the two disagree the document is the
//! referee, and the disagreement is a finding either way: either the store does
//! not do what was specified, or the specification does not say what was meant.
//! Copying the match into a second file would have been an expensive way to
//! compare a function with itself.
//!
//! # The generator's shape is the point
//!
//! The document's own §6 named the trap before any of this existed: with a *set*
//! of authorities, a misclassification is too **few** — a silent escalation the
//! compiler cannot see, because `{write}` type-checks exactly like
//! `{read, write}`. A generator that only asked about single-authority
//! statements would agree with any naive implementation and prove nothing about
//! the case the whole model exists for. So `CREATE`, `UPDATE` and `BACKUP` —
//! the three two-kind statements — are in the sample by construction, not by
//! chance.
//!
//! # The seeds are checked in
//!
//! A property test with a fresh random seed is a test that passes today. The
//! seeds below are an archive: a failure is reproducible by number, and a seed
//! that ever caught something is **appended** and never regenerated.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

/// Seeds this test runs, newest last.
///
/// Appended to, never replaced: a seed that once caught a disagreement is the
/// only cheap proof that the disagreement stays caught.
const SEEDS: [u64; 8] = [1, 2, 3, 5, 8, 13, 21, 34];

/// How many identities each seed builds.
const IDENTITIES: u32 = 12;

/// A whole number, from a sequence a seed reproduces exactly.
struct Rolling(u64);

impl Rolling {
    fn next(&mut self) -> u64 {
        // xorshift64*, which is here because a checked-in seed has to mean the
        // same sequence on every machine and `std` offers no such thing.
        let mut held = self.0;
        held ^= held >> 12;
        held ^= held << 25;
        held ^= held >> 27;
        self.0 = held;
        held.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// An index into a slice of `bound` entries.
    ///
    /// Written in `try_from` and `checked_rem` rather than `as` and `%` because
    /// the workspace refuses both, and a test is not an exception: a truncating
    /// cast in a generator would silently narrow the space it generates over.
    fn below(&mut self, bound: usize) -> usize {
        let Ok(bound) = u64::try_from(bound) else {
            return 0;
        };
        let picked = self.next().checked_rem(bound).unwrap_or(0);
        usize::try_from(picked).unwrap_or(0)
    }
}

/// A container an authority can be held at, as the language spells it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reach {
    Store,
    Namespace(&'static str),
    Database(&'static str, &'static str),
}

impl Reach {
    /// The `ON …` clause that names this container.
    fn named(self) -> String {
        match self {
            Self::Store => "STORE".to_owned(),
            Self::Namespace(name) => format!("NAMESPACE {name}"),
            Self::Database(namespace, name) => format!("DATABASE {namespace}.{name}"),
        }
    }

    /// Whether this container holds `inner` — downward only, per §4's chain,
    /// where a holding narrows to what it contains and never widens.
    fn contains(self, inner: Self) -> bool {
        match (self, inner) {
            (Self::Store, _) => true,
            (Self::Namespace(outer), Self::Namespace(name)) => outer == name,
            (Self::Namespace(outer), Self::Database(namespace, _)) => outer == namespace,
            (Self::Database(a, b), Self::Database(c, d)) => a == c && b == d,
            _ => false,
        }
    }

    /// The two-directional form `USE` needs: selecting `prod` is a step towards
    /// `prod.shop`, so a holding below the named container counts.
    fn touches(self, other: Self) -> bool {
        self.contains(other) || other.contains(self)
    }

    /// The tenancy a user declared at this reach carries.
    fn tenancy(self) -> Option<&'static str> {
        match self {
            Self::Store => None,
            Self::Namespace(name) | Self::Database(name, _) => Some(name),
        }
    }
}

/// Every reach a generated identity can hold something at.
const REACHES: [Reach; 5] = [
    Reach::Store,
    Reach::Namespace("prod"),
    Reach::Database("prod", "shop"),
    Reach::Namespace("staging"),
    Reach::Database("staging", "shop"),
];

/// The five authorities, spelled as the language spells them.
const KINDS: [&str; 5] = ["read", "write", "manage", "govern", "operate"];

/// Where a statement's authority must be held.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    /// The container the statement names.
    Reached,
    /// The whole store, which a user with a tenancy of their own never is.
    Store,
}

/// One statement, with the set the document assigns it.
struct Demand {
    /// What the session runs, after selecting `prod.shop`. `NNN` is replaced by
    /// the identity's ordinal, because a statement that declares a name
    /// succeeds once and then fails for a reason that is not a permission —
    /// which the first run of this test reported as thirteen disagreements. The
    /// created record's id is quoted, because an unquoted `1` followed by the
    /// ordinal lexes as a duration — a second way a test can fail for a reason
    /// that has nothing to do with permission.
    script: &'static str,
    /// The kinds §3 assigns. Empty means the statement demands nothing.
    kinds: &'static [&'static str],
    /// Where §4 says they must be held.
    at: At,
    /// `true` for `USE`, whose rule is *hold something here* rather than a kind.
    anything: bool,
}

/// The sample, one per class in §3, transcribed from the document.
///
/// The three two-kind statements are here deliberately: they are the only cases
/// that can distinguish a correct classification from a thin one.
const SAMPLE: [Demand; 11] = [
    Demand {
        script: "USE NAMESPACE prod;",
        kinds: &[],
        at: At::Reached,
        anything: true,
    },
    Demand {
        script: "SELECT * FROM orders;",
        kinds: &["read"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "UPSERT orders:1 = { total: 1 };",
        kinds: &["write"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "DELETE orders:2;",
        kinds: &["write"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "CREATE orders:'newNNN' = { total: 1 };",
        kinds: &["read", "write"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "UPDATE orders:1 SET total = 2;",
        kinds: &["read", "write"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "DEFINE TABLE probeNNN;",
        kinds: &["manage"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "DEFINE NAMESPACE freshNNN;",
        kinds: &["manage"],
        at: At::Store,
        anything: false,
    },
    Demand {
        script: "DEFINE USER extraNNN ON prod.shop ROLE viewer PASSWORD 'correct horse battery';",
        kinds: &["govern"],
        at: At::Reached,
        anything: false,
    },
    Demand {
        script: "INFO FOR NODE;",
        kinds: &["operate"],
        at: At::Store,
        anything: false,
    },
    Demand {
        script: "BACKUP;",
        kinds: &["read", "operate"],
        at: At::Store,
        anything: false,
    },
];

/// A generated identity: what it holds, and where it lives.
struct Identity {
    name: String,
    /// The container it was declared at, which is also its tenancy.
    home: Reach,
    /// Every `(kind, reach)` pair it holds.
    held: Vec<(&'static str, Reach)>,
}

impl Identity {
    /// Whether this identity holds `kind` at a container covering `reach`.
    fn permits(&self, kind: &str, reach: Reach) -> bool {
        self.held
            .iter()
            .any(|(held, at)| *held == kind && at.contains(reach))
    }

    /// Whether it holds anything on the path to `reach`, either direction.
    fn touches(&self, reach: Reach) -> bool {
        self.held.iter().any(|(_, at)| at.touches(reach))
    }
}

/// The second evaluator: the document's rules, and nothing else.
///
/// The script a test runs is `USE NAMESPACE prod; USE DATABASE shop; <statement>`,
/// so the answer is the conjunction of three decisions — the two selections and
/// the statement — because a script stops at its first refusal.
fn document_allows(who: &Identity, demand: &Demand) -> bool {
    let namespace = Reach::Namespace("prod");
    let database = Reach::Database("prod", "shop");

    // **The tenancy gate, which this test discovered rather than transcribed.**
    // §4 of the document writes the chain in terms of holdings alone, and on
    // that reading a user declared in `staging` who is granted `read` in `prod`
    // may select `prod`. The store refuses, and the store is right: a declared
    // tenancy is a second, independent confinement, and a tenant who could be
    // granted their way out of it would make `ON` decorative. The consequence —
    // that a grant outside a user's own tenancy is silently inert rather than
    // refused when it is made — is recorded as a question rather than decided
    // here. The document is the referee, and here it was incomplete.
    if !who.home.touches(namespace) || !who.home.touches(database) {
        return false;
    }

    // `USE` demands the weakest predicate that closes the existence oracle:
    // hold something here. §3, the `{any}` class.
    if !who.touches(namespace) || !who.touches(database) {
        return false;
    }
    if demand.anything {
        return true;
    }
    match demand.at {
        // A tenancy of one's own is what disqualifies from a store-reaching
        // statement: holding `prod.shop` means the store is not yours to act on.
        At::Store => {
            who.home.tenancy().is_none()
                && demand
                    .kinds
                    .iter()
                    .all(|kind| who.permits(kind, Reach::Store))
        }
        // §4: the set must be held at every container the statement reaches, and
        // every statement in the sample reaches the selected database.
        At::Reached => demand.kinds.iter().all(|kind| who.permits(kind, database)),
    }
}

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two namespaces, a table with a record in each, and a store owner.
fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders; CREATE orders:1 = { total: 5 };\n\
             DEFINE NAMESPACE staging; USE NAMESPACE staging;\n\
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
}

/// Build one identity in the store, and return what it holds.
fn declare(root: &mut Session<'_>, rolling: &mut Rolling, ordinal: u32) -> Identity {
    let name = format!("gen{ordinal}");
    let home = REACHES[rolling.below(REACHES.len())];
    let first = KINDS[rolling.below(KINDS.len())];
    let scope = match home {
        Reach::Store => String::new(),
        Reach::Namespace(named) => format!("ON NAMESPACE {named} "),
        Reach::Database(namespace, named) => format!("ON {namespace}.{named} "),
    };
    root.run(&format!(
        "DEFINE USER {name} {scope}AUTHORITIES {first} PASSWORD '{PASSWORD}';"
    ))
    .unwrap();
    let mut held = vec![(first, home)];

    // Between none and two further holdings, anywhere the store owner can hand
    // one out — which is everywhere they govern, and they govern the store.
    //
    // Everywhere except a tenancy this identity's own `ON` misses entirely: the
    // store refuses that grant rather than storing a holding nothing could ever
    // consult (Q-255). A refusal there is the store declining to build the
    // identity, not a disagreement about one, so the holding is dropped from the
    // model too — recording it would have the document assert an authority the
    // subject provably does not have.
    for _ in 0..rolling.below(3) {
        let kind = KINDS[rolling.below(KINDS.len())];
        let reach = REACHES[rolling.below(REACHES.len())];
        let asked = root.run(&format!("GRANT {kind} ON {} TO {name};", reach.named()));
        match asked {
            Ok(_) => held.push((kind, reach)),
            Err(refusal) if refusal.to_string().contains("confined") => {}
            Err(refusal) => panic!("the generator could not build {name}: {refusal}"),
        }
    }
    Identity { name, home, held }
}

#[test]
fn the_store_and_the_document_agree_about_every_generated_identity() {
    let mut disagreements = Vec::new();

    for seed in SEEDS {
        let store = store();
        peopled(&store);
        let mut root = Session::new(&store);
        root.sign_in("root", PASSWORD).unwrap();
        let mut rolling = Rolling(seed);

        for ordinal in 0..IDENTITIES {
            let who = declare(&mut root, &mut rolling, ordinal);
            for demand in &SAMPLE {
                let mut session = Session::new(&store);
                session.sign_in(&who.name, PASSWORD).unwrap();
                let unique = format!("{seed}x{ordinal}");
                let script = format!(
                    "USE NAMESPACE prod; USE DATABASE shop; {}",
                    demand.script.replace("NNN", &unique)
                );
                let answered = session.run(&script);
                let store_says = answered.is_ok();
                let document_says = document_allows(&who, demand);
                if store_says != document_says {
                    disagreements.push(format!(
                        "seed {seed}, {} holding {:?} at home {:?}: {:?} — store {}, document {}{}",
                        who.name,
                        who.held,
                        who.home,
                        demand.script,
                        if store_says { "allowed" } else { "refused" },
                        if document_says { "allowed" } else { "refused" },
                        answered
                            .err()
                            .map_or(String::new(), |why| format!(" ({why})")),
                    ));
                }
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "the store and the document disagree {} times:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}
