//! G057 C6: what the cluster's commits, replication and failover cost across
//! network distance.
//!
//! The distance is injected, not travelled: `benchmarks/cluster-distance.sh`
//! runs these in a Linux container and delays every packet to or from a peer
//! door by half the round trip (`tc netem` on loopback, filtered by the doors'
//! ports), so the nodes are that far from one another while the client stays
//! beside whichever node it asks. `TESSARIDB_TEST_RTT_MS` only labels the lines
//! each run prints; the runner writes them into the benchmark file.

use std::time::{Duration, Instant};

use tessari_wire::Client;

use super::{
    Band, a_cluster_declared, caught_up, into_item, into_orders, percentiles,
    the_node_a_majority_granted, the_node_that_takes, two_shard_leaders, what_the_nodes_said,
};

/// The three nodes whose commits, replication and failover are measured. The
/// runner delays the second address of each pair, the peer door.
const DISTANT: Band = [
    ("127.0.0.1:48050", "127.0.0.1:48051"),
    ("127.0.0.1:48052", "127.0.0.1:48053"),
    ("127.0.0.1:48054", "127.0.0.1:48055"),
];

/// The two shard leaders whose transactions across them are measured.
const DISTANT_LEADERS: Band = [
    ("127.0.0.1:48056", "127.0.0.1:48057"),
    ("127.0.0.1:48058", "127.0.0.1:48059"),
    ("127.0.0.1:48060", "127.0.0.1:48061"),
];

/// How many commits each latency is taken over.
const COMMITS: usize = 100;

/// The injected round trip this run is labelled with, in milliseconds.
fn rtt() -> String {
    std::env::var("TESSARIDB_TEST_RTT_MS").unwrap_or_else(|_| "0".to_owned())
}

#[test]
#[ignore = "a measurement across injected distance — G057 C6, run by \
            benchmarks/cluster-distance.sh, which sets the delay"]
fn commit_levels_replication_and_failover_across_injected_distance() {
    let rtt = rtt();
    let mut cluster = a_cluster_declared(&DISTANT, "", ["", "", ""]);
    let mut leader = the_node_a_majority_granted(&DISTANT);
    for index in 0..DISTANT.len() {
        caught_up(&DISTANT, index, "1", &cluster);
    }
    let followers: Vec<usize> = (0..DISTANT.len())
        .filter(|index| *index != leader)
        .collect();
    // The leader alone in its region: a local majority is the leader itself,
    // and a majority is one round trip away.
    let mut client = Client::connect(DISTANT[leader].0).unwrap();
    client
        .run(
            &format!(
                "ALTER REPLICA n{leader} REGION 'eu'; ALTER REPLICA n{} REGION 'us'; \
                 ALTER REPLICA n{} REGION 'us';",
                followers[0], followers[1]
            ),
            None,
        )
        .unwrap();
    for level in ["LEADER", "LOCAL MAJORITY", "MAJORITY"] {
        let taken: Vec<Duration> = (0..COMMITS)
            .map(|index| {
                let key = format!("{}{index:03}", level.replace(' ', "_").to_lowercase());
                let began = Instant::now();
                client
                    .run(&into_item(&key, &format!(" ACKNOWLEDGE {level}")), None)
                    .unwrap();
                began.elapsed()
            })
            .collect();
        let (p50, p99) = percentiles(taken);
        eprintln!(
            "DISTANCE rtt_ms={rtt} measure=commit level={} p50_us={p50} p99_us={p99} n={COMMITS}",
            level.replace(' ', "_")
        );
    }

    // From the leader's acknowledgement to the record readable on a follower.
    let follower = followers[0];
    let lag: Vec<Duration> = (0..20)
        .map(|index| {
            let key = format!("lag{index:02}");
            client
                .run(&into_item(&key, " ACKNOWLEDGE LEADER"), None)
                .unwrap();
            let acknowledged = Instant::now();
            while !super::item_ids_at(DISTANT[follower].0).is_ok_and(|held| held.contains(&key)) {
                assert!(
                    acknowledged.elapsed() < Duration::from_secs(60),
                    "the follower never received {key}{}",
                    what_the_nodes_said(&DISTANT, &cluster.logs)
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            acknowledged.elapsed()
        })
        .collect();
    let (p50, p99) = percentiles(lag);
    eprintln!("DISTANCE rtt_ms={rtt} measure=replication_lag p50_us={p50} p99_us={p99} n=20");
    drop(client);

    // A leader killed, until a survivor takes a write, three times.
    let mut took = Vec::new();
    for run in 0..3 {
        let before = format!("before{run}");
        let wrote = the_node_that_takes(&DISTANT, &[leader], &before, &cluster);
        for index in 0..DISTANT.len() {
            caught_up(&DISTANT, index, &before, &cluster);
        }
        drop(cluster.running[wrote].take());
        let killed = Instant::now();
        let survivors: Vec<usize> = (0..DISTANT.len()).filter(|index| *index != wrote).collect();
        let after = format!("after{run}");
        leader = the_node_that_takes(&DISTANT, &survivors, &after, &cluster);
        took.push(killed.elapsed());
        cluster.restart_with(wrote, &[]);
        caught_up(&DISTANT, wrote, &after, &cluster);
    }
    let (p50, _) = percentiles(took.clone());
    let worst = took.iter().max().copied().unwrap_or_default();
    eprintln!(
        "DISTANCE rtt_ms={rtt} measure=failover p50_us={p50} max_us={} n=3",
        worst.as_micros()
    );
}

#[test]
#[ignore = "a measurement across injected distance — G057 C6, run by \
            benchmarks/cluster-distance.sh, which sets the delay"]
fn a_commit_across_leaders_across_injected_distance() {
    let rtt = rtt();
    let cluster = two_shard_leaders(&DISTANT_LEADERS);
    let mut client = Client::connect(DISTANT_LEADERS[0].0).unwrap();
    // Warmed by an idempotent transaction, which may be refused while a
    // follower does not hold a shard's log yet, or answered in doubt.
    let warming = "USE NAMESPACE prod; USE DATABASE shop; BEGIN; \
                   UPSERT orders:'awarm' = { n: 1 }; UPSERT orders:'hwarm' = { n: 1 }; \
                   COMMIT ACROSS LEADERS;";
    let began = Instant::now();
    while client.run(warming, None).is_err() {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "no transaction across leaders ever committed{}",
            what_the_nodes_said(&DISTANT_LEADERS, &cluster.logs)
        );
        std::thread::sleep(super::POLL);
    }
    let mut refused = 0_usize;
    let mut last_refusal = String::new();
    let mut across = Vec::new();
    // Where the coordinator's log stood before the timed commits, so their
    // phases are read without the warming's (Q-931).
    let logged_before: Vec<u64> = cluster
        .logs
        .iter()
        .map(|log| std::fs::metadata(log).map_or(0, |held| held.len()))
        .collect();
    for index in 0..COMMITS {
        let script = format!(
            "USE NAMESPACE prod; USE DATABASE shop; BEGIN; \
             CREATE orders:'a{index:04}' = {{ n: 1 }}; CREATE orders:'h{index:04}' = {{ n: 1 }}; \
             COMMIT ACROSS LEADERS;"
        );
        let began = Instant::now();
        match client.run(&script, None) {
            Ok(_) => across.push(began.elapsed()),
            Err(why) => {
                refused = refused.saturating_add(1);
                last_refusal = why.to_string();
            }
        }
    }
    let one: Vec<Duration> = (0..COMMITS)
        .map(|index| {
            let began = Instant::now();
            if let Err(why) = client.run(&into_orders(&format!("b{index:04}")), None) {
                panic!(
                    "one write after {index} was refused: {why}{}",
                    what_the_nodes_said(&DISTANT_LEADERS, &cluster.logs)
                );
            }
            began.elapsed()
        })
        .collect();
    // Q-946: a renewal that opens with less than one round left of its lease
    // lands after the fence, and every write in between is refused.
    let mut least_left: Option<(usize, Duration)> = None;
    for (node, (log, from)) in cluster.logs.iter().zip(&logged_before).enumerate() {
        if let Some(left) = phases(&rtt, node, log, *from)
            && least_left.is_none_or(|(_, least)| left < least)
        {
            least_left = Some((node, left));
        }
    }
    let (across_p50, across_p99) = percentiles(across);
    let (one_p50, one_p99) = percentiles(one);
    eprintln!(
        "DISTANCE rtt_ms={rtt} measure=across_leaders p50_us={across_p50} p99_us={across_p99} \
         n={COMMITS} refused={refused}"
    );
    eprintln!(
        "DISTANCE rtt_ms={rtt} measure=one_write_majority p50_us={one_p50} p99_us={one_p99} \
         n={COMMITS}"
    );
    let round = Duration::from_millis(tessari_constants::ROUND_MILLIS);
    if let Some((node, left)) = least_left {
        assert!(
            left >= round,
            "n{node} opened a renewal with {left:?} of its lease left, less than the round's {round:?}"
        );
    }
    assert_eq!(
        refused,
        0,
        "transactions across leaders were refused, the last: {last_refusal}{}",
        what_the_nodes_said(&DISTANT_LEADERS, &cluster.logs)
    );
}

/// The phases of the timed commits as the coordinator logged them — each
/// part's prepare answered, here or by a remote leader, and each record
/// carried, by kind and by the link it rode — one `DISTANCE` line each (Q-931).
/// Nothing when the node was not asked to log them (`TESSARIDB_LOG`).
///
/// Answers the least lease any renewal round opened with (Q-946).
fn phases(rtt: &str, node: usize, log: &std::path::Path, from: u64) -> Option<Duration> {
    let read = std::fs::read(log).unwrap_or_default();
    let tail = read
        .get(usize::try_from(from).unwrap_or(usize::MAX)..)
        .unwrap_or_default();
    let field = |line: &str, name: &str| -> Option<String> {
        let named = format!("{name}=");
        let at = line.find(&named)?;
        line.get(at..)?
            .strip_prefix(&named)?
            .split_whitespace()
            .next()
            .map(|value| value.trim_matches('"').to_owned())
    };
    let mut timed: std::collections::BTreeMap<String, Vec<Duration>> =
        std::collections::BTreeMap::new();
    let mut least_left: Option<Duration> = None;
    for line in String::from_utf8_lossy(tail).lines() {
        let Some(micros) = field(line, "elapsed_us")
            .or_else(|| field(line, "waited_us"))
            .and_then(|us| us.parse::<u64>().ok())
        else {
            continue;
        };
        if line.contains("a renewal round ran") {
            // Q-946: the canvass, the lease left when it opened, and how old
            // the instant it was decided on was — per line and outcome.
            let line_of = if line.contains("range=Store") {
                "store"
            } else {
                "range"
            };
            let outcome = if line.contains("won=true") {
                "won"
            } else {
                "lost"
            };
            for (name, value) in [
                ("round", Some(micros)),
                (
                    "left_at_open",
                    field(line, "left_us").and_then(|us| us.parse().ok()),
                ),
                (
                    "decided_before_open",
                    field(line, "stale_us").and_then(|us| us.parse().ok()),
                ),
            ] {
                if let Some(value) = value {
                    let value = Duration::from_micros(value);
                    if name == "left_at_open" && least_left.is_none_or(|least| value < least) {
                        least_left = Some(value);
                    }
                    timed
                        .entry(format!("renewal_{line_of}_{outcome}_{name}"))
                        .or_default()
                        .push(value);
                }
            }
            continue;
        }
        let measure = if line.contains("a commit waited for its acknowledgement") {
            "acknowledgement_waited".to_owned()
        } else if line.contains("a cross-leader part answered") {
            match field(line, "local").as_deref() {
                Some("true") => "across_phase_local_part".to_owned(),
                _ => "across_phase_remote_part".to_owned(),
            }
        } else if line.contains("a reader asked a cross-leader record's leader") {
            "across_reader_asked_the_record".to_owned()
        } else if line.contains("a cross-leader record was carried") {
            format!(
                "across_carried_{}_{}",
                field(line, "what").unwrap_or_default(),
                field(line, "link").unwrap_or_default()
            )
        } else {
            continue;
        };
        timed
            .entry(measure)
            .or_default()
            .push(Duration::from_micros(micros));
    }
    for (measure, took) in timed {
        let n = took.len();
        let (p50, p99) = percentiles(took);
        eprintln!(
            "DISTANCE rtt_ms={rtt} measure=n{node}_{measure} p50_us={p50} p99_us={p99} n={n}"
        );
    }
    least_left
}
