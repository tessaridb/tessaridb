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
//! Seven tables, carrying **81** paths — the sum of the `expected` fields below,
//! which the assertion at the end of the test holds. That assertion is the
//! authority; this sentence is a copy of it.
//!
//! **And the copy has now been wrong twice.** It said **66** while the assertion
//! said 68. It was corrected to **73** while the assertion said **80** — so the
//! correction restored the habit and not the number, and the gap was wider
//! afterwards than before. W265 found it at 73/80 while adding the sixth frame
//! kind. The remedy written here after the first drift was *change the table,
//! change the assertion, change this line*, which is discipline; the assertion
//! held both times because it is checked, and this line failed both times
//! because it is not. **Whether a number in prose should exist at all when the
//! assertion beside it is derived is Q-585.**
//!
//! # The path added by the redirect frame, and how it was classified
//!
//! `frame::Kind::Elsewhere` is the sixth frame kind, and it is classified
//! **not a data path**. It carries no record, no catalog entry and no grant: its
//! whole body is an address, a node id, an epoch and one byte saying whether the
//! address is worth remembering. Everything in it is topology, and topology a
//! caller already reached this node to ask about.
//!
//! It is also, today, a kind this build never SENDS — both client readers refuse
//! it as meaningless on their connection.
//!
//! **Re-classification trigger:** the day a redirect names anything the receiver
//! could not otherwise learn — a namespace, a table, a tenant — it stops being
//! topology and becomes a disclosure, and the decision to send one has to be
//! reached through the same grant the read itself was. Today it names an
//! endpoint an operator declared and a node id already proved on the link.
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
        //
        // 19 since the campaign: `Db::leading` reads back the epoch a round
        // granted this node, which `Db::hold` installed beside the lease.
        // Classified **exempt on the same ground and one step weaker**, and the
        // weakening is worth naming rather than glossing: `hold` only writes
        // process memory, while `leading` READS it out, and what it reads now
        // travels — a greeting carries it to every peer, which is the whole
        // reason it exists. What travels is a single integer that the node
        // asserts about itself, it is carried on a mutually authenticated link,
        // and a peer that disbelieved it could check it against its own vote;
        // no record, no catalog entry and no grant is reachable through it.
        //
        // **Re-classification trigger:** the day a caller outside this node can
        // SET it — a management statement, an HTTP route, a peer frame that
        // writes another node's leadership — the fact stops being one this
        // process observed about itself and must be enforced where it is
        // written. Today the only writer is the campaign thread, from a round it
        // opened and counted itself.
        //
        // 21 since the leadership row (G025 **S1.2**): `Db::record_leadership`
        // and `Db::leader_of`. **This pair is NOT exempt on the ground the three
        // above share, and saying so is the point of the entry.** They take a
        // reach, and `record_leadership` writes a catalog entry and therefore a
        // log record that every subscribing follower applies. Everything that
        // made `hold`, `leading` and `hold_lease` exempt — no reach, no record,
        // process memory that is never persisted — is false of it.
        //
        // It is classified **ENFORCED**, by two things already in the path
        // rather than by a check added beside it:
        //
        //   * the **lease fence**. It commits an ordinary transaction, so a node
        //     past its fence is refused by the same rule that refuses every
        //     other write. A node that may not write may not record that it
        //     leads.
        //   * **its own identity**. There is no `node` argument: the id is read
        //     from this store's `node_identity`, the one fact ADR-0018 keeps out
        //     of the log precisely so it cannot be inherited. The only sentence
        //     this method can produce is *I took a leadership*, so no caller can
        //     write a row claiming somebody else leads.
        //
        // `leader_of` is a **read** of what was applied. It discloses a node id,
        // a range and an epoch — the same three facts a greeting already carries
        // to every peer on the link — and reads no record, no user, no grant.
        //
        // **Re-classification trigger, the same condition `hold` and `leading`
        // already carry and now half met:** a caller outside this process
        // reaches `record_leadership` the moment a management statement, an HTTP
        // route or a peer frame exposes it. Today the only caller is the
        // campaign thread in the binary, after a round it opened and counted
        // itself, under the guard that the epoch changed. The day that stops
        // being true the fence is not enough alone, and the grant model has to
        // reach this write.
        expected: 21,
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
        //
        // 28 since the campaign: `Store::leading` reads back the epoch handed to
        // `Store::hold`, whose signature grew to carry it. Classified **exempt**
        // for `Db::leading`'s reasons, recorded there in full rather than twice
        // — the facade method is this one and nothing else.
        //
        // What changed on the WRITE side is worth a line of its own: `hold` no
        // longer installs only a fence, it installs the identity of the
        // leadership that fence belongs to. Both arrive from one round and are
        // read by two different readers — the fence by every commit, the epoch
        // by every greeting — which is why they are stored together and neither
        // is derived from the other.
        //
        // 29 since the tail timeline (G024 **S6.1**, Q-542): `Store::mark_tail`
        // dates this node's own committed tail so a follower's copy has an age
        // rather than only a silence. Classified **exempt, on the narrowest
        // ground in this block and one that is worth stating precisely**: it
        // takes no reach and no identity, it reads a single number this node
        // already publishes through `committed_tail`, and what it writes is a
        // bounded ring of `(position, Instant)` in process memory that is never
        // persisted, never replicated and not reachable by any query.
        //
        // It is the write side of the pair whose read side is `follower_lag`,
        // and it shares that method's re-classification trigger rather than
        // inventing one: a caller who could call it at a time of their choosing
        // could **understate** how old a follower's copy is, and a bounded read
        // routed on that figure would then be served by a node the bound was
        // written to exclude. Nothing in this build offers that: its only caller
        // is the awareness cadence in the node binary, sampling a value it read
        // from its own store. The day a management statement, an HTTP route or a
        // peer frame can ask a node to date its tail, this becomes an
        // enforcement point and must be enforced where it is called.
        //
        // Worth recording beside the classification, because it is the reason
        // the method exists at all rather than a sample taken where the tail
        // moves: putting this on the commit path would have made it exact and
        // would have put a lock on the hottest path in the engine for the sake
        // of a diagnostic.
        // 30 since the election restriction (G024 **S7.1**, ADR-0063):
        // `Store::tail_leadership` answers which leadership wrote the record at
        // this node's committed tail. Classified **exempt, on the same ground as
        // `committed_tail` beside it**: it takes no reach and no identity, it
        // reads one epoch out of one log record the node already publishes the
        // position of, and it returns a number that is already on the wire in
        // every greeting this node sends.
        //
        // Its re-classification trigger is sharper than most in this block and
        // is worth stating, because the value is now a **safety input**: a voter
        // refuses a candidate whose log is behind its own, and this is the half
        // of that comparison the voter supplies about itself. A caller who could
        // make it answer LOWER than the truth could make a voter grant to a
        // candidate it should have refused, which is how leadership reaches a
        // node that does not hold the cluster's history. Nothing in this build
        // offers that: it is read-only, it is derived from the log on every
        // call, and its callers are the greeting path and this test.
        //
        // It is deliberately NOT `leading`, which sits above it and answers a
        // different question — the epoch a majority granted THIS node. Reading
        // that one into the comparison would rank a follower holding the newest
        // records below an ex-leader holding fewer, so the two are separate
        // methods rather than one with a flag.
        // 31 since the leadership gate (G024 **S7.1**, ADR-0064):
        // `Store::awaiting_leadership` answers whether this node takes part in
        // deciding and holds no leadership yet. Classified **enforced, and it is
        // the enforcement itself rather than a path to it** — it is one half of
        // the write gate, consulted by `effective_roles` (what a node reports)
        // and by `Transaction::settle` (what a node does), which is why it is a
        // free function underneath both rather than a rule written twice.
        //
        // Its re-classification trigger: anything that makes it answer `false`
        // where it should answer `true` re-opens a split-brain, because the two
        // nodes that lost a round would go on accepting writes the winner will
        // never see. The predicate is deliberately `COORDINATING` and not merely
        // *has no lease* — a store standing alone has no cluster to grant it
        // anything, and the global form stops every single-node deployment
        // writing, which is not a theory: deleting the role check fails 216 of
        // this crate's own tests.
        //
        // 32 since the quiet-cluster counter: `Store::campaigned` records that
        // this node stood in a leadership round. Classified **exempt, and it
        // reaches no data at all** — it takes no argument, returns nothing, and
        // increments one in-process atomic that is not persisted and never
        // leaves `Health`. It is here rather than in the serving process for the
        // reason `log_divergences` is: the scrape and `INFO FOR NODE` both read
        // the store's health, so a detector living anywhere else is one an
        // operator cannot see.
        //
        // Its re-classification trigger: giving it an argument, or making
        // anything read it back as a decision rather than as a report. A counter
        // that something BRANCHES on has stopped being a counter.
        //
        // 33 since the record version was separated from the log position
        // (Q-614, ADR-0073): `Store::committed_version` answers the version this
        // store has stamped up to. Classified **exempt, and it reaches no data**
        // — it takes no argument and returns one number read from a single META
        // key, with no tenancy anywhere in it. Its callers are `begin`,
        // `begin_at` and `retention_floor`, each of which needs the moment a
        // snapshot is taken at rather than any record written before it.
        //
        // Its re-classification trigger is the one B2 is about to pull: a
        // store-wide answer is safe only while one counter covers every range.
        // Make the counter per-range and a caller granted one namespace can
        // watch a number that moves with writes in namespaces it cannot read —
        // not the records, but their existence and their rate, which is a
        // channel rather than a leak and is exactly as invisible. The method
        // then owes a `Reach` and the answer owes the caller's own range.
        //
        // 34 since the log became per-range (G025 **S6.2**, Q-620):
        // `Store::homes` lists the logs this store holds, by scanning the
        // applied-position keys. Classified **exempt, and it reaches no data** —
        // it takes no argument and no identity, and it answers a list of
        // `Reach` values, which is the SHAPE of the store and not a record in
        // it. Its callers are the backup, which writes a section per log, and
        // the replay paths, which walk every log in order.
        //
        // Its re-classification trigger is precise and it is not the one the
        // method looks like it has: the existence of a namespace is already
        // published by the catalog to anybody who may read it, so the list is
        // not a new disclosure. What would change that is a caller who may read
        // NO namespace being handed one — so the day this is reachable from a
        // session rather than from a process that already holds the store, it
        // owes the caller's own reach.
        //
        // 35 since the same wave: `Store::apply_record_in` applies a record into
        // a log the caller names, for the one path that cannot derive it — a
        // selective subscriber is given records with everything outside its
        // reach removed, and a record emptied to nothing has no mutation left to
        // derive a home from (Q-621). Classified **not enforced here, and
        // enforced one layer up by the same gate as its neighbour**: its only
        // caller is `Store::apply_from_stream`, whose only caller is the peer
        // door, which asks `may_replicate` for the reach it then collects.
        //
        // Its re-classification trigger: a second caller. The argument above is
        // entirely about there being one, and a caller that names a log it was
        // not served from writes a range's records into another range's counter.
        //
        // 36-38 since the log gained its writer (G027 **S2.2**, Q-641). One
        // method left — `homes` — and four arrived, and they are classified
        // together because they answer one question the log's new name raises:
        // *which logs are there, and which of them is mine.*
        //
        // `Store::logs` replaces `homes` and is classified on exactly the
        // ground `homes` was, unchanged: it lists the logs something has been
        // written into, which the catalog already publishes the existence of.
        // `Store::logs_of` is the same list bounded to one home and inherits
        // the classification with it.
        //
        // `Store::writer` and `Store::own_log` are classified **exempt, and on
        // a narrower ground than anything above**: neither reads a record.
        // `writer` returns this node's own identifier, which is already in
        // every greeting this node sends and is `$node`'s answer to any caller
        // who may run a query at all; `own_log` pairs it with a `Reach` the
        // caller already holds. They disclose nothing the node does not publish
        // and they reach no data.
        //
        // Their re-classification trigger is the one the writer creates rather
        // than the one the list does: the day a caller can name ANOTHER node's
        // writer and be served that node's log, the enforcement point is
        // wherever that name is accepted — and it is `log_records` and
        // `log_records_within`, already classified above, that would owe the
        // check. Naming your own log is not a permission; being served somebody
        // else's is.
        expected: 38,
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
        expected: 6,
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
        // 7 since the file gained a section per log (G025 **S6.2**, Q-624):
        // `backup::only_log` answers the one log a sequence-bounded backup can
        // name, or refuses. Classified **exempt on the same ground as the rest
        // of this module** — it takes a store and no identity, and it answers a
        // `Reach` derived from `Store::homes`, which is the shape of the store
        // and not a record in it. The whole module is reached only by a process
        // that already holds the store.
        expected: 7,
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
    // 88 since the log became per-range: `Store::homes`, `Store::apply_record_in`
    // and `backup::only_log`, each classified in the block above (G025 S6.2).
    //
    // 91 since the log gained its writer: `Store::homes` became `Store::logs`,
    // and `logs_of`, `writer` and `own_log` joined it — a net three, each
    // classified in the block above (G027 S2.2, Q-641).
    assert_eq!(total, 91, "the counted tables no longer sum to 91");
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
/// position it does not hold — once per LOG now, because a node holds one log
/// per home and a position counts in one of them. It answers nobody: the value
/// leaves this process only as the `from` of an outgoing ask, and what comes
/// back is whatever the peer's own subscription check permits. The alternative
/// was to have the cursor read inside the collector, which would have put the
/// raw feed behind a network-facing type instead of in the process that owns
/// the store.
///
/// **Three and four.** `Serving::fill` is the peer door's scoped log reader —
/// the loop behind `Serving::collected` that fills one answer under a byte
/// budget, reading a page at a time so that a follower's uncapped record count
/// cannot make one read of the whole log — and `preceding` reads the one record
/// before the batch to state the leadership it follows. Both read at `over` — the subscription's own reach —
/// and both now also name the LOG the follower asked for, which the door has
/// already checked the subscription reaches. The two arguments answer two
/// questions and neither substitutes for the other: `over` is what this peer may
/// SEE, `log` is which counter the cursor counts in. They are still the only
/// callers of `log_records_within` in any crate this test scans. The second one earned its place here: it read at `Reach::Store` until
/// this rule was written, for an answer a scoped read gives identically, which
/// is how a rule acquires a hole nobody would have looked for. It is the
/// enforced path rather than a way past one: the reach it reads with is the
/// subscription the follower's own catalog row declares, so the permission and
/// the filter are one value and cannot disagree. Classified here rather than
/// left off the list, so that a SECOND caller appearing in these crates fails
/// this test — which is the property the one-caller argument rests on.
const CLASSIFIED: &[(&str, &str)] = &[
    // The same four paths as before the log became per-range; each now names the
    // log it reads — the home AND the writer that allocated into it since S2.2 —
    // which is a fact about the read and changes none of the classifications
    // above (Q-621, Q-641).
    (
        "tessari-cli/src/main.rs",
        "let tail = store.committed_tail(own).map_err(|why| why.to_string())?;",
    ),
    (
        "tessari-cli/src/main.rs",
        "let seed = match store.committed_tail(log) {",
    ),
    (
        "tessari-wire/src/collection.rs",
        ".log_records_within(over, log, cursor, room.min(COLLECTION_PAGE_RECORDS))",
    ),
    (
        "tessari-wire/src/collection.rs",
        ".log_records_within(over, log, before, 1)",
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

/// Where an epoch may be turned back into an [`Epoch`] from something that
/// already carried one, classified by the name of the function doing it.
///
/// Every permitted site is a **decoder**: it reads a number that some other node
/// or some earlier write already decided and rebuilds the type around it. None
/// of them invents one, which is why the rule can be a name rather than a list
/// of line numbers that goes stale on the next `cargo fmt`.
const DECODERS: [&str; 3] = ["decode", "from_value", "split_epoch"];

/// The one function allowed to CREATE an epoch, and the file it lives in.
///
/// `Renewing::once` computes `max(standing, stood, heard) + 1`. Both halves of
/// this pair are asserted: that nothing else creates one, and that this still
/// does — a ratchet whose subject has been renamed away passes by finding
/// nothing, which is the failure mode of every allow-list nobody re-reads.
const PRODUCER: (&str, &str) = ("tessari-wire/src/driver.rs", "once");

#[test]
fn the_campaign_is_the_only_place_an_epoch_is_created() {
    // G025 S3.3. An epoch is the cluster's count of leaderships, and its whole
    // value is that two nodes cannot hold one: a second place that allocates
    // one is two promotions handing out the same number, each with an honest
    // majority behind it and nothing anywhere in an error state.
    //
    // Inline test modules are excluded by truncating each file at its
    // `#[cfg(test)]`, because a fixture standing an epoch up by hand is exactly
    // what a test is for. That convention — tests last, in one module — is what
    // makes the truncation sound, and `cargo fmt` keeps it.
    let mut created = Vec::new();
    let mut found_producer = false;
    for entry in fs::read_dir(repo().join("crates")).expect("the crate directory") {
        let root = entry.expect("a crate").path().join("src");
        if !root.is_dir() {
            continue;
        }
        for path in sources(&root) {
            let text = fs::read_to_string(&path).unwrap();
            let production = text.split("#[cfg(test)]").next().unwrap_or_default();
            let shown = path.display().to_string();
            let lines: Vec<&str> = production.lines().collect();
            for (number, line) in lines.iter().enumerate() {
                if line.trim_start().starts_with("//") || !line.contains("Epoch::new(") {
                    continue;
                }
                let enclosing = lines[..=number]
                    .iter()
                    .rev()
                    .find_map(|above| {
                        above
                            .trim_start()
                            .trim_start_matches("pub ")
                            .trim_start_matches("const ")
                            .strip_prefix("fn ")
                            .map(|rest| rest.split('(').next().unwrap_or_default().to_owned())
                    })
                    .unwrap_or_default();
                if DECODERS.contains(&enclosing.as_str()) {
                    continue;
                }
                if shown.ends_with(PRODUCER.0) && enclosing == PRODUCER.1 {
                    found_producer = true;
                    continue;
                }
                created.push(format!(
                    "{shown}:{} in `{enclosing}` — {}",
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        created.is_empty(),
        "an epoch is created outside the campaign:\n  {}\n\n\
         An epoch is the cluster's count of leaderships and the campaign is the \
         only thing entitled to advance it: `Renewing::once` takes the highest \
         number this node holds, has stood for, or has heard granted, and adds \
         one. A second producer allocates a number somebody else may already \
         hold, which is two leaders with one epoch — the exact ambiguity the \
         epoch exists to remove. If the new site decodes a number that already \
         existed, put it in a function named for what it does ({}).",
        created.join("\n  "),
        DECODERS.join(", "),
    );
    assert!(
        found_producer,
        "`{}` no longer creates an epoch in {} — this test now passes by \
         finding nothing, which is not the same as the rule holding",
        PRODUCER.1, PRODUCER.0,
    );
}
