//! The metrics route: every surface's counts, and the process around them.

use super::{Answer, EXPOSITION};
use crate::basic::Presented;
use crate::tokens::Tokens;
use tessari_serve::{Census, Stopping};
use tessaridb::Db;

/// `GET /metrics` — the numbers, in the exposition format every scraper reads.
///
/// Plain text with a documented grammar, so it costs a function rather than a
/// dependency — which is the same trade the rest of this program makes and the
/// reason this format was chosen over any that needs a library to emit.
///
/// `# HELP` and `# TYPE` on every metric, because a scrape that describes itself
/// is the difference between a dashboard somebody can write and one that sends
/// them to read this file.
///
/// # What is absent, and why absent beats wrong
///
/// Without a [`Census`] — a node bound in-process, with no surrounding process
/// enumerating its surfaces — there is **no uptime line at all**, and the only
/// counters reported are this surface's own. The alternative would be to time
/// from this listener's own creation and call it uptime, which makes one metric
/// name mean two different things depending on how the node was started. A
/// scraper copes with a series that is missing; it cannot cope with one that
/// silently changes what it measures.
pub(crate) fn metrics(
    db: &Db,
    census: Option<&Census>,
    mine: &Stopping,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    // First, so a refused credential is answered before anything is written.
    let topics = match super::topics::topic_series(db, tokens, presented) {
        Ok(series) => series,
        Err(refused) => return refused,
    };
    let mut out = String::new();

    if let Some(census) = census {
        out.push_str("# HELP tessari_uptime_seconds How long this process has been running.\n");
        out.push_str("# TYPE tessari_uptime_seconds gauge\n");
        out.push_str(&format!(
            "tessari_uptime_seconds {:.3}\n",
            census.uptime().as_secs_f64()
        ));
    }

    // The store's own numbers, from the same call `/health` makes. A second way
    // to ask would be a second answer to drift from.
    if let Ok(held) = db.store().health() {
        out.push_str(
            "# HELP tessari_committed_sequence The last sequence the log has committed.\n",
        );
        out.push_str("# TYPE tessari_committed_sequence counter\n");
        out.push_str(&format!(
            "tessari_committed_sequence {}\n",
            held.committed.get()
        ));
        out.push_str("# HELP tessari_background_errors Failures in the engine's own threads.\n");
        out.push_str("# TYPE tessari_background_errors counter\n");
        out.push_str(&format!(
            "tessari_background_errors {}\n",
            held.background_errors
        ));
        // Any value above zero means this node was offered a record from a
        // leadership other than the one it applied at that position, and refused
        // it. It does not fall back to zero: the divergence an operator most
        // needs to see is the one that stopped happening on its own.
        out.push_str(
            "# HELP tessari_log_divergences Log positions another leadership tried to rewrite.\n",
        );
        out.push_str("# TYPE tessari_log_divergences counter\n");
        out.push_str(&format!(
            "tessari_log_divergences {}\n",
            held.log_divergences
        ));

        out.push_str("# HELP tessari_campaigns Leadership rounds this node has stood in.\n");
        out.push_str("# TYPE tessari_campaigns counter\n");
        out.push_str(&format!("tessari_campaigns {}\n", held.campaigns));
        // Absent rather than zero on a node holding no lease, because a series
        // that is always zero on every standalone store would train whoever
        // watches it to ignore the one reading that matters. When it is here it
        // is the split-brain signal: it heads toward zero, and zero while the
        // node is still accepting writes is the state the lease exists to
        // prevent.
        if let Some(left) = held.lease_remaining {
            out.push_str(
                "# HELP tessari_lease_remaining_seconds Writable time left under this node's lease.\n",
            );
            out.push_str("# TYPE tessari_lease_remaining_seconds gauge\n");
            out.push_str(&format!(
                "tessari_lease_remaining_seconds {}\n",
                left.as_secs_f64()
            ));
        }
    }

    replication(&mut out, db);

    // Worth a line of its own because it is the one number that says whether
    // the token bound is close: a node at `MAX_SESSION_TOKENS` starts refusing
    // sign-ins while every other counter here still reads healthy.
    out.push_str("# HELP tessari_sessions Session tokens this node is holding.\n");
    out.push_str("# TYPE tessari_sessions gauge\n");
    out.push_str(&format!("tessari_sessions {}\n", tokens.held()));

    out.push_str("# HELP tessari_connections Requests in flight, by surface.\n");
    out.push_str("# TYPE tessari_connections gauge\n");
    out.push_str("# HELP tessari_subscriptions Feeds open, by surface.\n");
    out.push_str("# TYPE tessari_subscriptions gauge\n");
    out.push_str("# HELP tessari_answers_total Answers written, refusals included.\n");
    out.push_str("# TYPE tessari_answers_total counter\n");
    out.push_str(
        "# HELP tessari_refusals_total Answers that were a failure rather than a result.\n",
    );
    out.push_str("# TYPE tessari_refusals_total counter\n");
    out.push_str("# HELP tessari_ready Whether the surface will take new work.\n");
    out.push_str("# TYPE tessari_ready gauge\n");

    match census {
        Some(census) => {
            for (name, stopping) in census.surfaces() {
                surface(&mut out, name, stopping);
            }
        }
        None => surface(&mut out, "http", mine),
    }

    out.push_str(&topics);
    Answer::text(200, out, EXPOSITION)
}

/// Where this node stands against the peer it collects from, and how far
/// behind each follower it serves is (ADR-0094 D4).
///
/// Both halves are absent until there is something to say: a node that follows
/// nobody has no state, and a leader nobody has collected from has no
/// followers. The state is one gauge per state with exactly one at 1, so a
/// dashboard can alert on `stranded` or `copy failed` without parsing a label.
fn replication(out: &mut String, db: &Db) {
    let store = db.store();
    if let Some(held) = store.upstream() {
        out.push_str(
            "# HELP tessari_replica_state Where this node stands against the peer it collects \
             from; the state it is in reads 1.\n",
        );
        out.push_str("# TYPE tessari_replica_state gauge\n");
        for state in tessaridb::Upstream::ALL {
            out.push_str(&format!(
                "tessari_replica_state{{state=\"{}\"}} {}\n",
                state.name(),
                u8::from(state == held.state)
            ));
        }
        out.push_str(
            "# HELP tessari_replica_copied_records Records installed by copies of the leader's \
             state.\n",
        );
        out.push_str("# TYPE tessari_replica_copied_records counter\n");
        out.push_str(&format!(
            "tessari_replica_copied_records {}\n",
            held.copied_records
        ));
    }
    let Ok(followers) = store.follower_lag() else {
        return;
    };
    if followers.is_empty() {
        return;
    }
    out.push_str(
        "# HELP tessari_follower_behind_records How many records each follower is short of \
         this node's tail.\n",
    );
    out.push_str("# TYPE tessari_follower_behind_records gauge\n");
    for follower in followers {
        out.push_str(&format!(
            "tessari_follower_behind_records{{node=\"{}\"}} {}\n",
            tessari_types::uuid_to_text(&follower.node),
            follower.behind
        ));
    }
}

/// One surface's five numbers, labelled by which surface it is.
pub(crate) fn surface(out: &mut String, name: &str, stopping: &Stopping) {
    // A label value is quoted and the names here are ours rather than a caller's,
    // so there is nothing to escape and no escaping written that would never run.
    out.push_str(&format!(
        "tessari_connections{{surface=\"{name}\"}} {}\n",
        stopping.requests()
    ));
    out.push_str(&format!(
        "tessari_subscriptions{{surface=\"{name}\"}} {}\n",
        stopping.feeds()
    ));
    out.push_str(&format!(
        "tessari_answers_total{{surface=\"{name}\"}} {}\n",
        stopping.answers()
    ));
    out.push_str(&format!(
        "tessari_refusals_total{{surface=\"{name}\"}} {}\n",
        stopping.refusals()
    ));
    out.push_str(&format!(
        "tessari_ready{{surface=\"{name}\"}} {}\n",
        u8::from(stopping.ready())
    ));
}
