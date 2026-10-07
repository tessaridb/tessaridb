//! What N subscriptions narrowed by a condition cost per commit (G069 C3,
//! ADR-0122 B4).
//!
//! # In process, on purpose
//!
//! Every feed today reads the log itself, so N feeds pay N log reads per commit,
//! and a condition adds one evaluation per change plus — for a change that does
//! not match — a read of the record as it stood before it. That cost lives in
//! [`Feed::round`], so the rounds are driven here directly, one thread, with no
//! socket between: a harness that held N connections would measure its own
//! buffers (KB `failure-mode-an-idle-feed-bench…`) before the node.
//!
//! # The rows
//!
//! For each N, one row per commit: the wall time from the commit until every
//! feed has run its round — the fan-out a node pays on its round thread for one
//! write. Each commit matches exactly one narrowed feed, so the other N − 1
//! judge a change that does not match and read the version before it. The same
//! N feeds watching the table with no condition are the control: what reading
//! the log and asking the authorities already costs.

use std::time::Instant;

use tessaridb::Db;
use tessaridb::feed::{Condition, Feed, Following, Round};

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How many subscriptions are measured.
const SUBSCRIBERS: [usize; 3] = [1, 100, 10_000];

/// How many commits are timed at each N.
const COMMITS: usize = 20;

/// N feeds on `chats`, each narrowed to its own chat or not narrowed at all,
/// and the time every one of them takes to read a commit.
///
/// # Errors
///
/// Returns whatever the store returns.
pub fn feeds(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench; DEFINE COLLECTION chats;")?;
    let mut reports = vec![Report::measurement(
        "corpus",
        &format!(
            "N in {SUBSCRIBERS:?} feeds on one table, {COMMITS} single-record commits timed at \
             each; a row is one commit's fan-out (every feed's round), in-process, one thread"
        ),
    )];
    for subscribers in SUBSCRIBERS {
        for narrowed in [false, true] {
            reports.push(fan_out(db, &mut session, subscribers, narrowed)?);
        }
    }
    Ok(reports)
}

/// Open `subscribers` feeds from the log's tail, then time `COMMITS` commits'
/// fan-out.
fn fan_out(
    db: &Db,
    session: &mut tessaridb::Session<'_>,
    subscribers: usize,
    narrowed: bool,
) -> Failable<Report> {
    let chats: Vec<tessaridb::Parameters> = (0..subscribers)
        .map(|at| {
            tessaridb::Parameters::from([(
                "chat".to_owned(),
                tessaridb::Value::String(format!("c{at}")),
            )])
        })
        .collect();
    let mut feeds = Vec::with_capacity(subscribers);
    for asked in &chats {
        let following = Following {
            from: tessaridb::Sequence::new(0),
            table: Some("chats"),
            cursor: None,
            condition: narrowed.then_some(Condition {
                text: "chat = $chat",
                parameters: asked,
            }),
        };
        feeds.push(Feed::open(db, session, &following)?);
    }
    // Read up to the present first, untimed: what is measured is one new
    // commit, never the backlog an earlier row left.
    for feed in &mut feeds {
        while feed.round(db, session, &mut |_, _, _, _| true)? != Round::Empty {}
    }
    let mut delivered = 0_usize;
    let mut samples = Samples::with_capacity(COMMITS);
    for commit in 0..COMMITS {
        let chat = commit.checked_rem(subscribers).unwrap_or(0);
        session.run(&format!(
            "CREATE chats = {{ chat: 'c{chat}', text: 'hi' }};"
        ))?;
        let started = Instant::now();
        for feed in &mut feeds {
            while feed.round(db, session, &mut |_, _, _, _| {
                delivered = delivered.saturating_add(1);
                true
            })? != Round::Empty
            {}
        }
        samples.push(started.elapsed());
    }
    // The control delivers every commit to every feed; a narrowed run, one.
    let expected = if narrowed {
        COMMITS
    } else {
        COMMITS.saturating_mul(subscribers)
    };
    if delivered != expected {
        return Err(format!("delivered {delivered} changes, expected {expected}").into());
    }
    let kind = if narrowed { "narrowed" } else { "table only" };
    Ok(samples.summarise(&format!(
        "{subscribers} feeds, {kind}: one commit's fan-out"
    )))
}
