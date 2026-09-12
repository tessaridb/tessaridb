//! The enforcement-point inventory, checked against the code it was derived
//! from.
//!
//! # Why a test and not a paragraph
//!
//! The permission coverage matrix was derived mechanically from the engine's own
//! tables — every route, every frame kind, every `pub fn` on `Db` and `Store`,
//! every command-line source — and each one was then classified as enforced,
//! exempt, or not a data path. That derivation was correct on the day it ran and
//! has no opinion about the next day: **a hand-maintained matrix decays at
//! exactly the rate new endpoints are added**, and nothing about adding a route
//! makes anybody open it.
//!
//! So the tables are counted here. A new HTTP route, a new frame kind, a new
//! `pub fn` on `Db` or `Store` fails this test until somebody has classified it
//! and moved the number — which is a thirty-second edit if the path is exempt
//! and the right conversation if it is not.
//!
//! # What is counted, and what is not
//!
//! Seven tables, carrying **73** paths. The original derivation counted 74 on
//! the day it ran. This line said **66** while the assertion below said 68 —
//! prose and number drifting apart is the very decay this test exists to catch,
//! and it had happened to the sentence describing the test. Both now come from
//! one place: change the table, change the assertion, change this line.
//!
//! # The path added by the vault, and how it was classified
//!
//! `Store::vault` is the sixty-first, and it is the one path here that is not
//! reached by a grant. Classified **enforced, by a second and independent
//! mechanism**: what it hands out is the `OpenVault`, and the key inside it is
//! lent for the duration of one call by `with_master` and cannot be kept. A
//! caller holding every grant the system offers and no passphrase is refused at
//! decryption; a caller holding the passphrase and no grant is refused at the
//! door by the ordinary reach check. Neither passes by the other's route, which
//! is criterion F3 of the vault goal stated as a coverage decision.
//!
//! # The path added by the effective role, and how it was classified
//!
//! `Store::effective_roles` is the twenty-third method on the substrate, and it
//! is classified **enforced, by adding no reach of its own**. It returns the
//! adopted role set minus `writable` when the lease has lapsed — a **subset** of
//! a value every caller that can reach it could already read from
//! `node_identity`, at the same place, through the same statement. It discloses
//! nothing new and grants nothing new, and the direction it can move a caller's
//! authority is downward.
//!
//! Its two callers are `INFO FOR NODE` and `$node`, both of which are statements
//! that have already passed the session's own checks before reaching it.
//!
//! # The path added by the copy's age, and how it was classified
//!
//! `Store::current_as_of` is the twenty-fourth method on the substrate, and it
//! is classified **enforced, by disclosing strictly less than its own input**.
//! It is a function of `effective_roles` and nothing else: it answers `Some(0)`
//! when that set carries `writable` and `None` otherwise, so every caller that
//! can reach it could already read the whole of the value it is derived from,
//! at the same place, through the same statement. A boolean fact about a value
//! already disclosed is not a disclosure.
//!
//! Its authority direction is downward for a second reason worth writing down.
//! Its one caller is the staleness check in `evaluate`, and the only thing that
//! check can do with the answer is **refuse** a read the engine would otherwise
//! have served. A path that can subtract a read and never add one cannot be
//! walked into more reach than the caller arrived with.
//!
//! # The path added by the cluster's lease, and how it was classified
//!
//! `Db::hold_lease` is the seventeenth method on the facade, and it is
//! classified **exempt — not a data path**, on two grounds that are worth
//! keeping apart.
//!
//! It reads and writes no record, no catalog entry and no grant: it hands the
//! store a span of time, and the only thing the store does with it is decide
//! when to stop accepting writes. Its effect on this node's authority is
//! monotone in the safe direction — a call to it can only ever make the node
//! write *less*.
//!
//! And it is not reachable from outside the process. No statement names it, no
//! HTTP route reaches it, no frame kind carries it; the caller is the code that
//! carried a leadership round to a majority. The exposure this classification
//! depends on is exactly that, so the day a statement or a route can take a
//! lease is the day this row is re-classified rather than re-counted — a
//! remotely settable lease is a remotely disabled fence.
//!
//! The classification is recorded here rather than only in the count, because
//! the count is a reminder and the classification is the work. The nine that are
//! not counted, named rather than quietly dropped, because a check that narrows
//! its own scope in silence is worse than one that never ran:
//!
//! - **the object-store fallthrough (6)** — its arms are `Method::` patterns
//!   nested inside the outer match's fallthrough arm, not a table; counting them
//!   would mean matching on indentation, which is a check on formatting;
//! - **`StatementKind` (1 row, 52 kinds)** — already guarded better than this
//!   could: `Needs::of` matches it exhaustively with no catch-all, so a new
//!   statement does not compile until it is classified;
//! - **the pre-match route (1)** and **the declared-consumer apply (1)** — one
//!   of each, with no table to grow.
//!
//! # The second assertion is the one that protects an exemption
//!
//! Twenty-five paths are exempt under **E3**: the embedded facade is a boundary
//! the permission system deliberately does not defend, because a caller holding
//! `&Store` holds every byte in it and no check stands between them. That
//! exemption is sound exactly while the surfaces that *are* on the network do
//! not use the facade to get around their own checks.
//!
//! Today both feed surfaces call the authorized `feed::follow` rather than the
//! raw `Db::poll`. Nothing stopped the next surface from doing otherwise, and
//! nothing would have noticed — the code would compile, the tests would pass, and
//! a subscription would stream records past every grant in the store.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn read(relative: &str) -> String {
    let path = repo().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The body of a top-level block, from its opening line to the `}` in column
/// zero that closes it.
///
/// Braces are not counted, deliberately: the doc comments in this tree are full
/// of `{ … }`, and a counter that walked them would be reading prose as code.
/// A top-level `impl` or `enum` closes at column zero, which is a property of
/// the formatter and is therefore checked by `cargo fmt` before it is relied on
/// here.
fn block(text: &str, header: &str) -> String {
    let opening = format!("\n{header} {{\n");
    let at = text
        .find(&opening)
        .unwrap_or_else(|| panic!("`{header}` is no longer where this check reads it"));
    let body = text
        .get(at.saturating_add(opening.len())..)
        .unwrap_or_default();
    let end = body
        .find("\n}\n")
        .unwrap_or_else(|| panic!("`{header}` never closes at column zero"));
    body.get(..end).unwrap_or_default().to_owned()
}

/// Lines whose trimmed form begins with `prefix`.
///
/// A doc line begins with `///` and an attribute with `#`, so neither is
/// counted, which is what lets this read a table without parsing Rust.
fn lines_beginning(text: &str, prefix: &str) -> usize {
    text.lines()
        .filter(|line| line.trim_start().starts_with(prefix))
        .count()
}

/// Public functions declared directly in a block.
fn public_functions(body: &str) -> usize {
    ["pub fn ", "pub const fn ", "pub async fn "]
        .iter()
        .map(|prefix| lines_beginning(body, prefix))
        .sum()
}

/// Public functions of a **module**, which are the ones at column zero.
///
/// The indentation is the distinction and it is not cosmetic: a `pub fn` inside
/// an `impl` is a method on a type the module happens to define, and a caller
/// reaches it only by already holding one of those. `feed.rs` is where this
/// matters — it exports one function and defines a small waiting type with two
/// methods of its own, and counting all three would put a condition variable in
/// the enforcement inventory.
fn module_functions(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("pub fn ") || line.starts_with("pub async fn "))
        .count()
}

/// Variants of an enum body: a four-space indent and a capital letter.
fn variants(body: &str) -> usize {
    body.lines()
        .filter(|line| {
            line.strip_prefix("    ")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
        })
        .count()
}

/// One table of enforcement points: where it lives, and how many it held when
/// the matrix classified them.
struct Table {
    file: &'static str,
    what: &'static str,
    expected: usize,
    count: fn(&str) -> usize,
}

const TABLES: &[Table] = &[
    Table {
        file: "crates/tessaridb/src/lib.rs",
        what: "public methods on `Db` — the embedded facade",
        // 18 since the grant seam: `Db::hold` installs a lease a majority
        // granted, whole, so the instant its round opened survives into the
        // fence. Classified **exempt** on exactly the ground `Db::hold_lease`
        // is: it takes no reach, it reads and writes no record, no catalog entry
        // and no grant, and what it touches is process memory that is never
        // persisted and never queryable.
        expected: 18,
        count: |text| public_functions(&block(text, "impl Db")),
    },
    Table {
        file: "crates/tessari-storage/src/store.rs",
        what: "public methods on `Store` — the substrate below the facade",
        // 15 since the vault: `Store::vault` hands out the per-process keyring.
        // Classified **enforced**, and by a different mechanism from every other
        // method here — the keyring is not reached by a grant but by holding a
        // passphrase, which is the point of criterion F3: reach and open are
        // separate, and neither passes by the other's route. What it hands out
        // is the `OpenVault` itself and never the key inside it; `with_master`
        // lends the key for one call and no caller can keep it.
        //
        // 16 since the audit trail: `Store::audit` hands out where a read of a
        // vault is recorded. Classified **exempt, and safe by construction
        // rather than by a boundary** — the only thing a caller can do with it
        // is install a device that must ALSO succeed for a read to be served.
        // There is no method on it that makes a read unrecorded, and the
        // built-in device is not in the list it exposes, so nothing reachable
        // through this can weaken the property it belongs to. That asymmetry is
        // the classification: a handle that can only tighten needs no gate.
        //
        // 17 since the cluster: `Store::apply_from_stream` writes a record that
        // arrived from a peer, after checking the predecessor it claims
        // (ADR-0059, G024 S1.2). Classified **not enforced here, and gated one
        // layer up by a gate that does not exist yet** — which is stated plainly
        // rather than filed as exempt, because the difference matters. It takes
        // no identity and applies no grant, exactly like `apply_record` beside
        // it, and that is correct at this layer: a replicated write was already
        // authorized where it was issued, and re-deciding it on the follower
        // would let two nodes reach different verdicts about one record. What
        // must be enforced is **who may open a stream at all**, and that debt is
        // now half paid. S4.1 shipped the reading half: `Session::replicate_from`
        // is the one authorized door to the log, and a peer holding no
        // `replicate` authority is refused there rather than here. That is the
        // right side for it — the leader decides who may take its log, and a
        // follower re-deciding a write it has already accepted is the divergence
        // this layer exists to avoid.
        //
        // The half still owed is the other direction: **which peers this store
        // will accept a stream FROM**. Nothing above `apply_from_stream` asks
        // that question yet, and it is G024 **S4.2** — the inter-node link is
        // mutually authenticated and a certificate for the wrong role cannot
        // join as a peer. Until it ships, the only thing standing here is that
        // the method is reachable solely by a process that already holds the
        // store handle, which is a property of the binary and not a permission.
        // Re-pointed rather than ticked off, because S4.1 closing does not make
        // this path enforced.
        //
        // 18 since the selective stream: `Store::log_records_within` reads the
        // log for a subscription and drops the mutations outside its reach
        // (G024 S3.2). Classified **not enforced here, and enforced one layer up
        // by a gate that now exists** — which is a different sentence from the
        // one above it and the difference is the whole point. It takes a `Reach`
        // and no identity, so on its own it will filter for anybody who can name
        // one; what makes that safe is that the only caller in the workspace is
        // `Session::replicate_from`, which asks `may_replicate` for **the same
        // `Reach` value** before it reads. One value answering both halves is
        // what makes the check and the filter unable to disagree: there is no
        // state in which a caller authorized for one namespace is served
        // another, because there is only one namespace named.
        //
        // The reason it is public at all is that a follower is a separate
        // process from the session that will one day feed it, and the crate
        // boundary is where that split lands. The reason it is not a second
        // enforcement point is that adding an identity check here would be the
        // change feed's mistake run backwards — two places deciding one
        // question, and the one that drifts is the one nobody is reading.
        //
        // 20 since per-follower lag (G024 **S6.1**). Two paths, both classified
        // **exempt, and by two different arguments** — which is why they are
        // written out separately rather than counted together.
        //
        // `Store::follower_lag` reports no store content at all: no record, no
        // catalog entry, no grant. It answers how far behind each follower is,
        // which is the same class of question `Store::health` answers about the
        // engine, and it is exempt on the same ground — the permission on it
        // belongs to the statement above it, and `INFO FOR NODE` is refused to a
        // viewer and to an editor by tests that say so.
        //
        // `Store::follower_served` is exempt on a narrower ground: it discloses
        // nothing because it returns nothing, and it mutates nothing in the
        // store because what it writes is process memory that is never
        // persisted and never read back by any query. Its one caller is
        // `Session::replicate_from`, *after* `may_replicate` has already been
        // asked — so a caller who reaches it has by construction already passed
        // the check that governs the log.
        //
        // What would change both classifications is the same event: a follower
        // row that carried anything about the CONTENT a follower received
        // rather than how much of it. It carries a node id, a position and two
        // measurements, and none of those is a record.
        //
        // 22 since the lease fence (G024 **S5.1**). `Store::hold_lease` and
        // `Store::lease_spent` are classified **exempt, and this pair is the
        // clearest case in the table**: neither takes a reach, neither reads or
        // writes a record, a catalog entry or a grant, and what they touch is
        // process memory that is never persisted and never queryable. The
        // permission question they raise is *who may grant leadership*, and that
        // question has no caller yet — nothing in this build takes a lease but a
        // test, because granting is a cluster act over a wire that does not
        // exist.
        //
        // Recorded here rather than deferred, because the day that wire lands is
        // the day `hold_lease` becomes an enforcement point of the first
        // importance: a caller who can take a lease can take leadership. Its
        // exemption is therefore **conditional on having no remote caller**, and
        // that condition is written down so the next wave meets it rather than
        // inherits it.
        //
        // 25 since the grant seam: `Store::hold` installs a lease a majority
        // granted, WHOLE, so the instant its round opened reaches the fence
        // instead of being restarted at installation. It is classified **exempt
        // on the same ground and under the same condition** — no reach, no
        // record, no catalog entry, no grant, and process memory that is never
        // persisted and never queryable.
        //
        // It is also the method that condition was written for. `hold` is the
        // shape a wire will call, and it is deliberately the one that carries
        // the whole lease, so when a driver opens rounds on a timer this pair is
        // where "who may grant leadership" stops being hypothetical. The
        // condition has moved one link along that chain. A round is now opened
        // by something that is not a test: `Standing::renew` decides when the
        // margin is spent and puts the ballot to every peer, and it is ordinary
        // code behind the `server` feature rather than a fixture. What is still
        // missing is its caller — nothing but a test calls `renew`, because the
        // thread that would open rounds on a timer has not been written. The
        // exemption therefore still holds, on a ground one step narrower than
        // the one it held on before.
        //
        // 26 and 27 since the follower: `Store::collected` records what this
        // node collected for ITSELF and whether the answer brought it level, and
        // `Store::collection` reads that back. Classified **exempt, and on a
        // narrower ground than the pair above**, because neither touches a
        // record. `collected` writes two facts about this process — a position
        // and an instant — into memory that does not survive it, and
        // `collection` reads them; no data leaves the node through either, and
        // no identity could be checked at this layer that is not already checked
        // where the collection was authorized, which is the peer link's mutual
        // TLS and the leader's own `replicate` grant.
        //
        // What they DO decide is whether a bounded read may be answered here at
        // all, because `current_as_of` is derived from them — so a caller that
        // could set them arbitrarily could make a stale copy look current and
        // collect reads the bound was written to send elsewhere. That is why the
        // exemption is written down rather than assumed, and it names its own
        // re-classification trigger: **the moment either becomes reachable from
        // a network surface — a management statement, an HTTP route, a peer
        // frame that lets one node write another's currency — this stops being
        // a process-local fact and must be enforced.** Today the only caller is
        // `Collector::collect`, which sets them from what it itself observed on
        // a link it itself opened.
        expected: 27,
        count: |text| public_functions(&block(text, "impl Store")),
    },
    Table {
        file: "crates/tessari-cli/src/arguments.rs",
        what: "what the command line can be asked to do",
        expected: 10,
        count: |text| variants(&block(text, "pub enum Source")),
    },
    Table {
        file: "crates/tessari-wire/src/frame.rs",
        what: "frame kinds the binary protocol accepts",
        expected: 5,
        count: |text| variants(&block(text, "pub(crate) enum Kind")),
    },
    Table {
        file: "crates/tessari-http/src/lib.rs",
        what: "routes matched by method and path",
        expected: 8,
        count: |text| lines_beginning(text, "(Method::"),
    },
    Table {
        file: "crates/tessari-backup/src/lib.rs",
        what: "the backup surface, which takes a store and no identity",
        expected: 6,
        count: module_functions,
    },
    Table {
        file: "crates/tessaridb/src/feed.rs",
        what: "the authorized change feed",
        expected: 1,
        count: module_functions,
    },
];

#[test]
fn every_enforcement_point_table_holds_what_the_coverage_matrix_classified() {
    let mut moved = Vec::new();
    let mut total = 0;
    for table in TABLES {
        let found = (table.count)(&read(table.file));
        total += found;
        if found != table.expected {
            moved.push(format!(
                "{}: {} — {} now, {} when they were classified",
                table.file, table.what, found, table.expected
            ));
        }
    }
    assert!(
        moved.is_empty(),
        "{} enforcement-point table(s) changed size:\n  {}\n\n\
         Every one of these is a place a caller reaches this store. Classify the \
         new path — enforced, or exempt under a named boundary — in the coverage \
         matrix, then move the number here. Do not move the number first: the \
         count is the reminder, and the classification is the work.",
        moved.len(),
        moved.join("\n  "),
    );
    assert_eq!(total, 75, "the counted tables no longer sum to 75");
}

/// Every `.rs` file under a directory.
fn sources(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        panic!("{} is not readable", directory.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|kind| kind == "rs") {
            found.push(path);
        }
    }
    found
}

/// The raw change feed on the embedded facade, and the authorized call that
/// exists instead of each.
///
/// Not the whole E3 list. `Db::names_in`, `Db::store` and `Db::writable_peer`
/// are on it too and are called from these crates today — legitimately, because
/// none of them has an authorized alternative to reach for: `names_in` resolves
/// ids the caller already holds, `store` is the named gateway the facade exists
/// to expose, and `writable_peer` reports topology. Forbidding those would fail
/// on the current tree, and a rule that has to be suppressed on the day it is
/// written teaches everybody to suppress it.
///
/// These four are different: each has an enforced replacement, and reaching past
/// it is never anything but a mistake.
const RAW_FEED: &[&str] = &[
    ".poll(",
    ".changes_since(",
    ".subscribe(",
    ".committed_tail(",
    // Added the day the peer door began serving records. It is the scoped log
    // reader, so it takes a reach and no identity — on its own it will filter
    // for anybody who can name one — and the whole safety of that arrangement
    // is that exactly one caller in a networked crate reaches it. A rule is what
    // keeps "exactly one" true next year.
    ".log_records_within(",
];

/// Call sites classified as exempt, by file and by the EXACT line.
///
/// Three entries, and each one is a judgement about what that line discloses
/// rather than about who wrote it.
///
/// **One.** The peer door's greeting says how far this node's log reaches,
/// which is a field of the handshake frame — and `committed_tail` is the only
/// call that answers it.
///
/// **Classified exempt, and on what it discloses rather than on who calls it.**
/// It returns a `Sequence`: one position, no record, no field, no value from any
/// tenant. The three calls beside it on the list — `poll`, `changes_since`,
/// `subscribe` — hand back RECORDS, which is what the whole assertion is about:
/// *reaching past `feed::follow` streams records past every grant in the store*.
/// A position number streams nothing, and a peer learns it anyway the moment it
/// collects.
///
/// **It is also disclosed to a stranger by nobody.** The greeting is written
/// only after a mutually authenticated handshake against a certificate this
/// cluster issued for the peer link; a connection that proves nothing never
/// reaches a frame.
///
/// **Matched on the exact line for a reason.** Any OTHER use of
/// `committed_tail` in that file still fails this test, and editing this line
/// withdraws its exemption — so the classification cannot drift away from the
/// code it classified without somebody noticing.
///
/// **Re-classification trigger, and half of it has now fired.** The door DOES
/// serve a collection as of the wave that added the two entries below, and the
/// exemption survives it: what a collection hands over is decided by the
/// follower's subscription, not by what it knew about the tail, so a position
/// number buys a peer nothing it could not already ask for. The other half
/// stands unchanged — the day a tail is answered to anything that has not
/// completed the peer handshake, this is wrong again.
///
/// **Two.** The follower's collection loop reads its OWN tail to know the first
/// position it does not hold. It answers nobody: the value leaves this process
/// only as the `from` of an outgoing ask, and what comes back is whatever the
/// peer's own subscription check permits. The alternative was to have the
/// cursor read inside the collector, which would have put the raw feed behind a
/// network-facing type instead of in the process that owns the store.
///
/// **Three and four.** `Serving::collected` is the peer door's scoped log
/// reader, and `preceding` reads the one record before the batch to state the
/// leadership it follows. Both read at `over` — the subscription's own reach —
/// and they are the only callers of `log_records_within` in any crate this test
/// scans. The second one earned its place here: it read at `Reach::Store` until
/// this rule was written, for an answer a scoped read gives identically, which
/// is how a rule acquires a hole nobody would have looked for. It is the
/// enforced path rather than a way past one: the reach it reads with is the
/// subscription the follower's own catalog row declares, so the permission and
/// the filter are one value and cannot disagree. Classified here rather than
/// left off the list, so that a SECOND caller appearing in these crates fails
/// this test — which is the property the one-caller argument rests on.
const CLASSIFIED: &[(&str, &str)] = &[
    (
        "tessari-cli/src/main.rs",
        "let tail = store.committed_tail().map_err(|why| why.to_string())?;",
    ),
    (
        "tessari-cli/src/main.rs",
        "let held = match store.committed_tail() {",
    ),
    (
        "tessari-wire/src/collection.rs",
        ".log_records_within(over, asked.from, limit)",
    ),
    (
        "tessari-wire/src/collection.rs",
        ".log_records_within(over, before, 1)",
    ),
];

#[test]
fn no_serving_surface_reaches_the_raw_change_feed() {
    let mut reached = Vec::new();
    for crate_name in ["tessari-http", "tessari-wire", "tessari-cli"] {
        let root = repo().join("crates").join(crate_name).join("src");
        for path in sources(&root) {
            let text = fs::read_to_string(&path).unwrap();
            for (number, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let shown = path.display().to_string();
                let classified = CLASSIFIED
                    .iter()
                    .any(|(file, exact)| shown.ends_with(file) && line.trim() == *exact);
                if classified {
                    continue;
                }
                for call in RAW_FEED {
                    if line.contains(call) {
                        reached.push(format!("{shown}:{} {}", number + 1, line.trim()));
                    }
                }
            }
        }
    }
    assert!(
        reached.is_empty(),
        "a surface on the network reaches the unfiltered change feed:\n  {}\n\n\
         `feed::follow` is the authorized call and does the same job: it asks \
         whether this session may read, narrows the tables to the ones it was \
         granted, redacts the fields it was not, and re-asks all three on every \
         poll round so a revocation ends the subscription. Reaching past it \
         streams records past every grant in the store, and nothing raises an \
         error while it happens.",
        reached.join("\n  "),
    );
}
