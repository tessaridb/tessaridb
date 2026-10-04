//! How good full-text search is on a real corpus, measured against graded
//! judgments (ADR-0100 D3).
//!
//! `cargo run --release -p tessari-bench --example relevance -- <content> <judgments> [--repeat n]`
//!
//! `<content>` is the documentation site's `content/` directory. It is cut the
//! way the site cuts it — one fragment per heading, its `text` the page title,
//! the heading and the body — and declared the way the site declares it, so a
//! number here describes the search a reader of that site gets. `<judgments>` is
//! a tab-separated file: a kind (`word`, `prefix` or `fuzzy`), a query, and
//! `page=grade` pairs, graded 0 to 3; a page not listed is graded 0.
//!
//! # What it reports
//!
//! NDCG@10 and MRR@10 per kind and over every query, a line per query so two
//! runs can be compared query by query, and latency: the first run of each
//! query is reported as cold and the next `repeat` as warm, each at the
//! nearest-rank p50 and p99. Results are passages; they are judged as pages,
//! keeping the first passage of each page, so a page answered twice is not
//! counted twice.
//!
//! The corpus is named by a fingerprint over its paths and bytes, printed with
//! every run, because the directory is not under version control here and two
//! runs over different corpora are not a comparison.
//!
//! # What makes it refuse
//!
//! A judgment naming a page the corpus does not hold. Read as grade 0, a typo
//! in the judgments would quietly lower every measure it touched, forever.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tessaridb::{Db, Parameters, Value};

/// Results per query that are judged.
const DEPTH: usize = 10;
/// Passages read per query: enough to find `DEPTH` distinct pages.
const READ: usize = 50;
/// The lowest grade that counts as relevant for the reciprocal rank.
const RELEVANT: u8 = 2;

/// The site's own declarations (`docs-store/src/schema.rs`), less the parts
/// search does not read.
const SCHEMA: &str =
    "DEFINE NAMESPACE docs; USE NAMESPACE docs; DEFINE DATABASE site; USE DATABASE site;
DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer;
DEFINE COLLECTION fragment;
DEFINE FIELD text ON fragment TYPE string ANALYZER english;
DEFINE INDEX by_text ON fragment FIELDS text SEARCH;";

struct Fragment {
    page: String,
    title: String,
    heading: String,
    body: String,
    text: String,
}

struct Judged {
    kind: String,
    query: String,
    grades: BTreeMap<String, u8>,
}

/// The front matter's title, and the body after it.
fn split_front(source: &str) -> (String, &str) {
    let Some(rest) = source.strip_prefix("+++") else {
        return (String::new(), source);
    };
    let Some((front, body)) = rest.split_once("\n+++") else {
        return (String::new(), source);
    };
    let title = front
        .lines()
        .find_map(|line| line.strip_prefix("title = "))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .unwrap_or_default();
    (title, body)
}

/// One page cut at every heading outside a code fence.
fn fragments_of(page: &str, source: &str, into: &mut Vec<Fragment>) {
    let (title, body) = split_front(source);
    let mut heading = title.clone();
    let mut lines: Vec<&str> = Vec::new();
    let mut fenced = false;
    let mut flush = |heading: &str, lines: &mut Vec<&str>| {
        if !lines.iter().all(|line| line.trim().is_empty()) || heading != title {
            into.push(Fragment {
                page: page.to_owned(),
                title: title.clone(),
                heading: heading.to_owned(),
                body: lines.join("\n"),
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

/// Every markdown file under `root`, sorted, with its page name.
fn pages(root: &Path) -> Result<Vec<(String, PathBuf)>, Box<dyn Error>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                let name = path
                    .strip_prefix(root)?
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned();
                found.push((name, path));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// FNV-1a over the sorted names and bytes.
fn fingerprint(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn judgments(path: &Path, held: &BTreeSet<String>) -> Result<Vec<Judged>, Box<dyn Error>> {
    let mut judged = Vec::new();
    for (line, number) in fs::read_to_string(path)?.lines().zip(1_usize..) {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut columns = line.split('\t');
        let (Some(kind), Some(query), Some(pairs)) =
            (columns.next(), columns.next(), columns.next())
        else {
            return Err(format!("line {}: expected kind, query, grades", number).into());
        };
        if !["word", "prefix", "fuzzy"].contains(&kind) {
            return Err(format!("line {}: unknown kind {kind}", number).into());
        }
        let mut grades = BTreeMap::new();
        for pair in pairs.split_whitespace() {
            let (page, grade) = pair
                .split_once('=')
                .ok_or_else(|| format!("line {}: {pair} is not page=grade", number))?;
            if !held.contains(page) {
                return Err(format!("line {}: no page {page} in the corpus", number).into());
            }
            let grade: u8 = grade.parse()?;
            if grade > 3 {
                return Err(format!("line {}: grade {grade} is above 3", number).into());
            }
            grades.insert(page.to_owned(), grade);
        }
        judged.push(Judged {
            kind: kind.to_owned(),
            query: query.to_owned(),
            grades,
        });
    }
    Ok(judged)
}

/// The searches the `--engine` mode reads (ADR-0105): the fragment's title,
/// heading and body as three fields of one document, weighed equally or with
/// the two short fields above the body.
const ENGINE: &str = "DEFINE SEARCH flat ON fragment FIELDS title, heading, body ANALYZER english;
DEFINE SEARCH weighted ON fragment FIELDS title WEIGHT 2, heading WEIGHT 3, body ANALYZER english;";

fn statement(kind: &str, engine: Option<&str>) -> String {
    if let Some(search) = engine {
        return match kind {
            "fuzzy" => format!("SELECT page FROM SEARCH {search} MATCHES FUZZY $q LIMIT {READ};"),
            _ => format!("SELECT page FROM SEARCH {search} MATCHES $q LIMIT {READ};"),
        };
    }
    match kind {
        "word" => format!(
            "SELECT page FROM fragment WHERE text MATCHES $q ORDER BY search::score(text, $q) DESC LIMIT {READ};"
        ),
        // What a reader part-way through a word is asking: the words before it
        // whole, the last one a prefix, ranked (ADR-0104).
        "prefix" => format!(
            "SELECT page FROM fragment WHERE text MATCHES $q ORDER BY search::score(text, $q) DESC LIMIT {READ};"
        ),
        // Ranked by what the fuzzy read reached, each term weighed by its
        // distance (G058 C3); before that a misspelling scored `0` and this
        // read ran in store order.
        _ => format!(
            "SELECT page FROM fragment WHERE text MATCHES FUZZY $q ORDER BY search::score(text, $q) DESC LIMIT {READ};"
        ),
    }
}

/// The first `DEPTH` distinct pages an answer reaches, in its order.
fn ranked_pages(
    session: &mut tessaridb::Session<'_>,
    judged: &Judged,
    engine: Option<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut parameters = Parameters::new();
    let query = if judged.kind == "prefix" {
        format!("{}*", judged.query)
    } else {
        judged.query.clone()
    };
    parameters.insert("q".to_owned(), Value::String(query));
    let outcomes = session.run_with(&statement(&judged.kind, engine), &parameters)?;
    let records = outcomes
        .last()
        .and_then(|outcome| outcome.records())
        .unwrap_or_default();
    let mut seen = Vec::new();
    for (_, value) in records {
        if let Value::Object(fields) = value
            && let Some(Value::String(page)) = fields.get("page")
            && !seen.contains(page)
        {
            seen.push(page.clone());
        }
        if seen.len() == DEPTH {
            break;
        }
    }
    Ok(seen)
}

fn discount(rank: usize) -> f64 {
    // `rank` is at most `DEPTH`, which an f64 holds exactly.
    1.0 / f64::from(u32::try_from(rank).unwrap_or(u32::MAX).saturating_add(2)).log2()
}

fn gain(grade: u8) -> f64 {
    f64::from((1_u32 << grade).saturating_sub(1))
}

fn ndcg(pages: &[String], grades: &BTreeMap<String, u8>) -> f64 {
    if pages.is_empty() {
        return 0.0;
    }
    let found: f64 = pages
        .iter()
        .enumerate()
        .map(|(rank, page)| gain(grades.get(page).copied().unwrap_or(0)) * discount(rank))
        .sum();
    let mut ideal: Vec<u8> = grades.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let best: f64 = ideal
        .iter()
        .take(DEPTH)
        .enumerate()
        .map(|(rank, grade)| gain(*grade) * discount(rank))
        .sum();
    if best > 0.0 { found / best } else { 0.0 }
}

fn reciprocal_rank(pages: &[String], grades: &BTreeMap<String, u8>) -> f64 {
    pages
        .iter()
        .position(|page| grades.get(page).copied().unwrap_or(0) >= RELEVANT)
        .map_or(0.0, |rank| {
            1.0 / f64::from(u32::try_from(rank).unwrap_or(u32::MAX).saturating_add(1))
        })
}

/// The nearest-rank percentile: a latency that was observed.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (p.saturating_mul(sorted.len())).div_ceil(100).max(1);
    sorted[rank.saturating_sub(1).min(sorted.len().saturating_sub(1))]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (Some(content), Some(judged_at)) = (arguments.first(), arguments.get(1)) else {
        return Err(
            "usage: relevance <content dir> <judgments.tsv> [--repeat n] [--engine flat|weighted]"
                .into(),
        );
    };
    let option = |name: &str| {
        arguments
            .iter()
            .position(|held| held == name)
            .and_then(|at| arguments.get(at.saturating_add(1)))
            .cloned()
    };
    let repeat: usize = match option("--repeat") {
        Some(count) => count.parse()?,
        None => 5,
    };
    let engine = option("--engine");
    if engine
        .as_deref()
        .is_some_and(|search| !matches!(search, "flat" | "weighted"))
    {
        return Err("--engine is `flat` or `weighted`".into());
    }

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut fragments = Vec::new();
    let found = pages(Path::new(content))?;
    for (name, path) in &found {
        let source = fs::read_to_string(path)?;
        fingerprint(&mut hash, name.as_bytes());
        fingerprint(&mut hash, source.as_bytes());
        fragments_of(name, &source, &mut fragments);
    }
    let held: BTreeSet<String> = found.into_iter().map(|(name, _)| name).collect();
    let judged = judgments(Path::new(judged_at), &held)?;

    let db = Db::in_memory()?;
    let mut session = db.session();
    session.run(SCHEMA)?;
    for (number, fragment) in fragments.iter().enumerate() {
        let mut parameters = Parameters::new();
        parameters.insert("id".to_owned(), Value::from(i64::try_from(number)?));
        parameters.insert("page".to_owned(), Value::String(fragment.page.clone()));
        parameters.insert(
            "heading".to_owned(),
            Value::String(fragment.heading.clone()),
        );
        parameters.insert("text".to_owned(), Value::String(fragment.text.clone()));
        parameters.insert("title".to_owned(), Value::String(fragment.title.clone()));
        parameters.insert("body".to_owned(), Value::String(fragment.body.clone()));
        session.run_with(
            "CREATE fragment:$id = { page: $page, title: $title, heading: $heading, \
             body: $body, text: $text };",
            &parameters,
        )?;
    }
    if engine.is_some() {
        session.run(ENGINE)?;
    }

    let build = if cfg!(debug_assertions) {
        "DEBUG (not a measurement)"
    } else {
        "release"
    };
    println!(
        "build {} {build} · {}-{} · memory store · corpus {:016x}: {} pages, {} fragments · {} queries · {}",
        tessaridb::BUILD_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        hash,
        held.len(),
        fragments.len(),
        judged.len(),
        engine.as_deref().map_or_else(
            || "field index".to_owned(),
            |search| format!("DEFINE SEARCH {search}")
        )
    );

    let mut by_kind: BTreeMap<&str, (f64, f64, u32)> = BTreeMap::new();
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    for query in &judged {
        let started = Instant::now();
        let pages = ranked_pages(&mut session, query, engine.as_deref())?;
        cold.push(started.elapsed());
        for _ in 0..repeat {
            let started = Instant::now();
            let again = ranked_pages(&mut session, query, engine.as_deref())?;
            warm.push(started.elapsed());
            if again != pages {
                return Err(format!("{} answered two orders on two runs", query.query).into());
            }
        }
        let (n, r) = (
            ndcg(&pages, &query.grades),
            reciprocal_rank(&pages, &query.grades),
        );
        println!(
            "{:<6} {:<28} ndcg {n:.3} rr {r:.3}  {}",
            query.kind,
            query.query,
            pages.join(" ")
        );
        for kind in [query.kind.as_str(), "all"] {
            let entry = by_kind.entry(kind).or_insert((0.0, 0.0, 0));
            entry.0 += n;
            entry.1 += r;
            entry.2 = entry.2.saturating_add(1);
        }
    }
    for (kind, (n, r, count)) in &by_kind {
        let count = f64::from(*count);
        println!(
            "{kind:<6} NDCG@{DEPTH} {:.4}  MRR@{DEPTH} {:.4}  ({count} queries)",
            n / count,
            r / count
        );
    }
    cold.sort_unstable();
    warm.sort_unstable();
    for (label, samples) in [("cold", &cold), ("warm", &warm)] {
        println!(
            "{label}  p50 {:.3} ms  p99 {:.3} ms  ({} runs)",
            milliseconds(percentile(samples, 50)),
            milliseconds(percentile(samples, 99)),
            samples.len()
        );
    }
    Ok(())
}
