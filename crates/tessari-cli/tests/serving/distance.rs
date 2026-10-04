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
    assert_eq!(
        refused,
        0,
        "transactions across leaders were refused, the last: {last_refusal}{}",
        what_the_nodes_said(&DISTANT_LEADERS, &cluster.logs)
    );
}
