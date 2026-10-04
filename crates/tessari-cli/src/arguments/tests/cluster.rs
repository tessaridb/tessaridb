use super::*;

/// The four cluster flags that take a path or an address, with `--serve`
/// because they need it. `--seed` is left to the caller, since it is the
/// repeatable one.
fn told_a_cluster(extra: &[&str]) -> Vec<String> {
    let mut given = vec![
        "--serve".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--cluster-credential".to_owned(),
        "leaf.pem".to_owned(),
        "--cluster-key".to_owned(),
        "key.pem".to_owned(),
        "--cluster-authority".to_owned(),
        "ca.pem".to_owned(),
        "--cluster-address".to_owned(),
        "0.0.0.0:9081".to_owned(),
    ];
    given.extend(extra.iter().map(|held| (*held).to_owned()));
    given
}

#[test]
fn a_node_told_nothing_about_a_cluster_is_the_node_it_is_today() {
    let held = asked(&["--serve", "127.0.0.1:0"]).expect("serving alone");
    assert!(
        held.cluster.is_none(),
        "absent is the single node, not a defect"
    );
}

#[test]
fn a_node_told_every_part_keeps_each_path_and_every_seed() {
    let given = told_a_cluster(&["--seed", "one.example:9080", "--seed", "two.example:9080"]);
    let held = parse(given.into_iter()).expect("all five parts");
    let cluster = held.cluster.expect("told about a cluster");
    assert_eq!(cluster.chain, std::path::PathBuf::from("leaf.pem"));
    assert_eq!(cluster.key, std::path::PathBuf::from("key.pem"));
    assert_eq!(cluster.authority, std::path::PathBuf::from("ca.pem"));
    assert_eq!(cluster.door, "0.0.0.0:9081", "its own door's address");
    assert_eq!(
        cluster.seeds,
        vec!["one.example:9080".to_owned(), "two.example:9080".to_owned()],
        "--seed is repeatable and keeps its order"
    );
}

#[test]
fn a_node_told_half_a_cluster_is_refused_before_it_starts() {
    let refused = parse(
        [
            "--serve",
            "127.0.0.1:0",
            "--cluster-credential",
            "leaf.pem",
            "--seed",
            "one.example:9080",
        ]
        .iter()
        .map(|held| (*held).to_owned()),
    )
    .expect_err("half a cluster is not a configuration");
    let (given, missing) = refused
        .split_once("but not")
        .expect("the refusal separates what was given from what was missing");
    assert!(given.contains("a peer credential"), "names what was given");
    assert!(missing.contains("a private key"), "names what was missing");
    assert!(missing.contains("a cluster authority"), "names both gaps");
    assert!(missing.contains("a peer address"), "and the address too");
}

#[test]
fn a_node_told_where_to_dial_but_not_where_to_answer_is_refused() {
    // The asymmetric one: four flags are about reaching somebody else and
    // this one is about being reachable, so it is the part an operator
    // forgets without noticing. A node missing it would dial its seeds,
    // learn the cluster, and be a member nothing could ever call back.
    let given: Vec<String> = told_a_cluster(&["--seed", "one.example:9080"])
        .into_iter()
        .filter(|held| held != "--cluster-address" && held != "0.0.0.0:9081")
        .collect();
    let refused = parse(given.into_iter()).expect_err("a cluster with nowhere to be reached at");
    let (given, missing) = refused
        .split_once("but not")
        .expect("the refusal separates what was given from what was missing");
    assert!(missing.contains("a peer address"), "names what was missing");
    assert!(given.contains("seed addresses"), "names what was given");
}

#[test]
fn a_cluster_address_with_nothing_after_it_is_refused_rather_than_swallowing_the_next_flag() {
    let refused = asked(&["--serve", "127.0.0.1:0", "--cluster-address"])
        .expect_err("a flag that wants a value and got none");
    assert!(
        refused.contains("--cluster-address wants a host:port"),
        "{refused}"
    );
}

#[test]
fn a_cluster_credential_without_anything_to_serve_is_refused() {
    let refused = parse(
        ["--health", "--cluster-credential", "leaf.pem"]
            .iter()
            .map(|held| (*held).to_owned()),
    )
    .expect_err("a value silently dropped is a value somebody believes was used");
    assert!(
        refused.contains("serves nothing"),
        "says why, not merely that: {refused}"
    );
}

#[test]
fn a_seed_with_no_address_after_it_is_refused_rather_than_swallowing_the_next_flag() {
    let refused = asked(&["--serve", "127.0.0.1:0", "--seed"])
        .expect_err("a flag that wants a value and got none");
    assert!(
        refused.contains("--seed wants <node-id>@<host:port>"),
        "{refused}"
    );
}
