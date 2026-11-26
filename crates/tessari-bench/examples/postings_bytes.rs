//! What the search index's postings cost on the documentation corpus, and
//! what a packed posting-list codec would save (G102 C9).
//!
//! `cargo run --release -p tessari-bench --example postings_bytes -- <content>`
//!
//! `<content>` is the documentation site's `content/` directory, cut into
//! fragments the way the site cuts it. The example builds the site's own
//! index over them, then reports:
//!
//! - the index keyspace's bytes, split into postings and everything else;
//! - what the same postings would cost as packed lists — per term, its
//!   document ordinals delta-bitpacked and its frequencies bitpacked, the
//!   standard block-codec shape — with a 10 % block-overhead allowance;
//! - warm latency of an index-served `MATCHES` for the corpus's commonest
//!   terms, p50 over repeats.
//!
//! The verdict rule is the wave's: a change is adopted only at a 20 % win.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KeyRange, Keyspace, KvBackend, MemoryBackend, ScanRequest};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::{Analyzer, Filter, Language};

/// The site's own declarations, as `relevance.rs` carries them.
const SCHEMA: &str =
    "DEFINE NAMESPACE docs; USE NAMESPACE docs; DEFINE DATABASE site; USE DATABASE site;
DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;
DEFINE COLLECTION fragment;
DEFINE FIELD text ON fragment TYPE string ANALYZER english;
DEFINE INDEX by_text ON fragment FIELDS text SEARCH;";

/// The postings key kind's tag — the first byte of every key (`key-grammar`
/// §2), `0x12` in the index keyspace.
const POSTING_TAG: u8 = 0x12;

struct Fragment {
    text: String,
}

fn split_front(source: &str) -> (String, &str) {
    if let Some(rest) = source.strip_prefix("+++\n")
        && let Some(end) = rest.find("\n+++\n")
    {
        let head = &rest[..end];
        let title = head
            .lines()
            .find_map(|line| line.strip_prefix("title = \""))
            .and_then(|line| line.strip_suffix('"'))
            .unwrap_or_default()
            .to_owned();
        return (title, rest.get(end.saturating_add(5)..).unwrap_or_default());
    }
    (String::new(), source)
}

/// One page cut at its headings, the way the site cuts it.
fn fragments_of(source: &str, into: &mut Vec<Fragment>) {
    let (title, body) = split_front(source);
    let mut heading = title.clone();
    let mut lines: Vec<&str> = Vec::new();
    let mut fenced = false;
    let mut flush = |heading: &str, lines: &mut Vec<&str>| {
        if !lines.iter().all(|line| line.trim().is_empty()) || heading != title {
            into.push(Fragment {
                text: format!("{title}\n{heading}\n{}", lines.join("\n")),
            });
        }
        lines.clear();
    };
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let marks = line.chars().take_while(|c| *c == '#').count();
        if !fenced && (1..=6).contains(&marks) && line[marks..].starts_with(' ') {
            flush(&heading, &mut lines);
            heading = line[marks..].trim().to_owned();
        } else {
            lines.push(line);
        }
    }
    flush(&heading, &mut lines);
}

fn pages(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Bits one value of `largest` needs, at least one.
fn bits(largest: u64) -> u64 {
    u64::from(64_u32.saturating_sub(largest.leading_zeros())).max(1)
}

/// A TessariQL string literal.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('\'');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::args()
        .nth(1)
        .ok_or("usage: postings_bytes <content-dir>")?;
    let mut fragments = Vec::new();
    for path in pages(Path::new(&root))? {
        fragments_of(&fs::read_to_string(&path)?, &mut fragments);
    }
    println!("fragments: {}", fragments.len());

    let backend = Arc::new(MemoryBackend::new());
    let shared: Arc<dyn KvBackend> = backend.clone();
    let store = Store::open(shared).now()?;
    let mut session = Session::new(&store);
    session.run_now(SCHEMA)?;
    for (at, fragment) in fragments.iter().enumerate() {
        session.run_now(&format!(
            "CREATE fragment:{at} = {{ text: {} }};",
            quote(&fragment.text)
        ))?;
    }

    // The stored shape: every index-keyspace pair, postings told apart by the
    // kind tag every key leads with.
    let pairs = backend
        .scan(&ScanRequest::new(Keyspace::INDEX, KeyRange::all()))
        .now()?;
    let (mut posting_pairs, mut posting_key_bytes, mut posting_value_bytes) = (0_u64, 0_u64, 0_u64);
    let (mut other_pairs, mut other_bytes) = (0_u64, 0_u64);
    for (key, value) in &pairs {
        let bytes = key.as_slice();
        let size =
            u64::try_from(bytes.len().saturating_add(value.as_slice().len())).unwrap_or(u64::MAX);
        if bytes.first() == Some(&POSTING_TAG) {
            posting_pairs = posting_pairs.saturating_add(1);
            posting_key_bytes =
                posting_key_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            posting_value_bytes = posting_value_bytes
                .saturating_add(u64::try_from(value.as_slice().len()).unwrap_or(u64::MAX));
        } else {
            other_pairs = other_pairs.saturating_add(1);
            other_bytes = other_bytes.saturating_add(size);
        }
    }
    let posting_bytes = posting_key_bytes.saturating_add(posting_value_bytes);

    // The packed shape, from the same analysis the index ran: per term, the
    // document ordinals delta-bitpacked and the frequencies bitpacked, an
    // eight-byte header per term and a tenth again for block framing.
    let analyzer = Analyzer::new(vec![
        Filter::Lowercase,
        Filter::Ascii,
        Filter::Stemmer(Language::English),
    ]);
    let mut terms: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    for (ordinal, fragment) in fragments.iter().enumerate() {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for term in analyzer.terms(&fragment.text) {
            let held = counts.entry(term).or_insert(0);
            *held = held.saturating_add(1);
        }
        for (term, frequency) in counts {
            terms
                .entry(term)
                .or_default()
                .push((u64::try_from(ordinal).unwrap_or(u64::MAX), frequency));
        }
    }
    let mut packed: u64 = 0;
    for postings in terms.values() {
        let mut largest_delta = 1_u64;
        let mut largest_frequency = 1_u64;
        let mut previous = 0_u64;
        for (ordinal, frequency) in postings {
            largest_delta = largest_delta.max(ordinal.saturating_sub(previous));
            largest_frequency = largest_frequency.max(*frequency);
            previous = *ordinal;
        }
        let id_bits = bits(largest_delta);
        let frequency_bits = bits(largest_frequency);
        let body = (id_bits.saturating_add(frequency_bits))
            .saturating_mul(u64::try_from(postings.len()).unwrap_or(u64::MAX))
            .div_ceil(8);
        packed = packed
            .saturating_add(8)
            .saturating_add(body)
            .saturating_add(u64::try_from(postings.len().div_ceil(8)).unwrap_or(0));
    }
    let packed_with_overhead = packed.saturating_add(packed / 10);

    // Warm latency of an index-served term read, for the commonest terms.
    let mut commonest: Vec<(&String, usize)> = Vec::new();
    for (term, held) in &terms {
        commonest.push((term, held.len()));
    }
    commonest.sort_by(|one, other| other.1.cmp(&one.1).then_with(|| one.0.cmp(other.0)));
    let mut latencies = Vec::new();
    for (term, _) in commonest.iter().take(5) {
        let read = format!(
            "SELECT text FROM fragment WHERE text MATCHES {t} LIMIT 10;",
            t = quote(term)
        );
        session.run_now(&read)?;
        let mut held = Vec::new();
        for _ in 0..50 {
            let started = Instant::now();
            session.run_now(&read)?;
            held.push(started.elapsed());
        }
        held.sort();
        latencies.push(((*term).clone(), held[held.len() / 2]));
    }

    println!("\nindex keyspace pairs: {}", pairs.len());
    println!(
        "postings: {posting_pairs} entries, {posting_bytes} bytes \
         ({posting_key_bytes} key + {posting_value_bytes} value)"
    );
    println!("other index entries: {other_pairs}, {other_bytes} bytes");
    println!("terms: {}", terms.len());
    println!("packed-list model: {packed_with_overhead} bytes (incl. 10% block overhead)");
    let saved = posting_bytes.saturating_sub(packed_with_overhead);
    // Tenths of a percent, in integers: a measurement prints what it can
    // divide exactly.
    let tenths = saved
        .saturating_mul(1000)
        .checked_div(posting_bytes)
        .unwrap_or(0);
    let percent = format!("{}.{}", tenths / 10, tenths % 10);
    println!("postings bytes a packed codec would save: {saved} ({percent}%)");
    for (term, median) in &latencies {
        println!("MATCHES '{term}' p50 warm: {median:?}");
    }
    println!(
        "\nverdict rule: adopt a packed codec only at a 20% win — this model says {percent}% \
         of the postings bytes, before the read path's own costs"
    );
    Ok(())
}
