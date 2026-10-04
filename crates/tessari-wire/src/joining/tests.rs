#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// A seed in the form the flag now takes, `<node-id>@<host:port>`.
const ONE_SEED: &str = "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a@one.example:9080";
const TWO_SEED: &str = "2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b@two.example:9080";

/// A PEM authority and a PEM leaf it signed, minted in memory.
///
/// In memory for the reason the peer link's own fixture gives: a fixture on
/// disk is key material in a repository, and one with an expiry date is a
/// test that fails on a day nobody chose.
struct Pem {
    authority: String,
    leaf: String,
    key: String,
}

fn minted() -> Pem {
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let authority_key = rcgen::KeyPair::generate().unwrap();
    let authority = params.self_signed(&authority_key).unwrap();

    let leaf_params = rcgen::CertificateParams::new(vec!["a.peer.tessari".to_owned()]).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = leaf_params
        .signed_by(&leaf_key, &authority, &authority_key)
        .unwrap();

    Pem {
        authority: authority.pem(),
        leaf: leaf.pem(),
        key: leaf_key.serialize_pem(),
    }
}

/// Where a test node's own door would bind. Any address will do — nothing
/// in this module binds one; that is `link.rs`'s and the binary's work.
const DOOR: &str = "0.0.0.0:9081";

fn at(name: &str) -> PathBuf {
    PathBuf::from(name)
}

fn parsed(pem: &Pem) -> Result<Joining> {
    Joining::parse(
        CredentialFile {
            bytes: pem.leaf.as_bytes(),
            path: &at("leaf.pem"),
        },
        CredentialFile {
            bytes: pem.key.as_bytes(),
            path: &at("key.pem"),
        },
        CredentialFile {
            bytes: pem.authority.as_bytes(),
            path: &at("ca.pem"),
        },
        DOOR.to_owned(),
        vec![ONE_SEED.to_owned()],
    )
}

#[test]
fn a_node_told_none_of_it_is_a_node_that_is_not_in_a_cluster() {
    let told = Told::from_parts(None, None, None, None, Vec::new()).unwrap();
    assert!(
        told.is_none(),
        "absent is the unclustered node, not a failure"
    );
}

#[test]
fn a_node_told_some_of_it_does_not_start() {
    let failure = Told::from_parts(
        Some(at("leaf.pem")),
        None,
        Some(at("ca.pem")),
        Some(DOOR.to_owned()),
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("half a cluster is refused");
    let said = failure.to_string();
    let (given, missing) = said
        .split_once("but not")
        .expect("the refusal separates what was given from what was missing");
    assert!(missing.contains("a private key"), "names what was missing");
    assert!(given.contains("a peer credential"), "names what was given");
}

#[test]
fn a_cluster_that_names_no_seed_is_a_configuration_the_store_completes() {
    // The founding node, and the single-node deployment being clustered.
    // Both have somewhere to be reached and nowhere to be reached FROM, and
    // refusing them here refuses the one node that needs no seed at all
    // (Q-577). Whether this node can actually reach anybody is a question
    // about the seeds OR the catalog, and only `serve` can see both.
    let told = Told::from_parts(
        Some(at("leaf.pem")),
        Some(at("key.pem")),
        Some(at("ca.pem")),
        Some(DOOR.to_owned()),
        Vec::new(),
    )
    .expect("four parts and no seed is a cluster configuration")
    .expect("all four given");
    assert!(
        told.seeds.is_empty(),
        "no seed was named and none is invented"
    );
    assert_eq!(told.door, DOOR, "the address its own door binds");
}

#[test]
fn a_seed_on_its_own_is_still_half_a_cluster() {
    // The other side of the same relaxation: a seed cannot COMPLETE a
    // configuration, so an operator who passed only `--seed` must be told
    // their four other flags never arrived rather than quietly starting a
    // node that is not in a cluster at all.
    let failure = Told::from_parts(None, None, None, None, vec![ONE_SEED.to_owned()])
        .expect_err("a seed alone is not a cluster configuration");
    let said = failure.to_string();
    let (given, missing) = said
        .split_once("but not")
        .expect("the refusal separates what was given from what was missing");
    assert!(given.contains("seed addresses"), "names what was given");
    assert!(
        missing.contains("a peer credential") && missing.contains("a peer address"),
        "names the parts that never arrived, not just the first of them"
    );
}

#[test]
fn a_node_told_all_of_it_keeps_every_path_it_was_given() {
    let told = Told::from_parts(
        Some(at("leaf.pem")),
        Some(at("key.pem")),
        Some(at("ca.pem")),
        Some(DOOR.to_owned()),
        vec![ONE_SEED.to_owned(), TWO_SEED.to_owned()],
    )
    .unwrap()
    .expect("all five given");
    assert_eq!(told.chain, at("leaf.pem"));
    assert_eq!(told.key, at("key.pem"));
    assert_eq!(told.authority, at("ca.pem"));
    assert_eq!(told.door, DOOR, "the address its own door binds");
    assert_eq!(told.seeds.len(), 2, "both seeds, in the order given");
}

#[test]
fn a_cluster_with_nowhere_to_be_reached_is_half_configured() {
    // The part an operator is likeliest to forget, because the other four
    // are all about reaching somebody ELSE and this one is about being
    // reachable. Told everything but this, a node would dial its seeds,
    // learn the cluster, and be a member nothing could ever call back.
    let failure = Told::from_parts(
        Some(at("leaf.pem")),
        Some(at("key.pem")),
        Some(at("ca.pem")),
        None,
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("a peer address is a part like the others");
    let said = failure.to_string();
    let (given, missing) = said
        .split_once("but not")
        .expect("the refusal separates what was given from what was missing");
    assert!(missing.contains("a peer address"), "names what was missing");
    assert!(given.contains("seed addresses"), "names what was given");
}

#[test]
fn a_credential_that_reads_carries_its_chain_its_key_and_one_authority() {
    let pem = minted();
    let joining = parsed(&pem).expect("a well-formed configuration");
    assert_eq!(joining.mine.chain.len(), 1, "the leaf");
    assert_eq!(
        joining.seeds,
        vec![Seed {
            node: [0x1a; NODE_ID_LEN],
            endpoint: "one.example:9080".to_owned(),
        }],
        "the seed is held as the pair it names, not as the text it was given"
    );
    assert!(
        !joining.authority.as_ref().is_empty(),
        "the authority's own bytes"
    );
}

#[test]
fn a_credential_file_holding_no_certificate_is_refused_by_content_not_by_path() {
    let pem = minted();
    let failure = Joining::parse(
        CredentialFile {
            bytes: b"this file exists and is not a certificate\n",
            path: &at("leaf.pem"),
        },
        CredentialFile {
            bytes: pem.key.as_bytes(),
            path: &at("key.pem"),
        },
        CredentialFile {
            bytes: pem.authority.as_bytes(),
            path: &at("ca.pem"),
        },
        DOOR.to_owned(),
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("an empty chain is not a credential");
    let said = failure.to_string();
    assert!(said.contains("leaf.pem"), "names the file");
    assert!(said.contains("held no certificate"), "a content problem");
}

#[test]
fn a_key_file_holding_no_key_is_refused_and_the_refusal_quotes_nothing() {
    // The authority's own certificate stands in the key slot deliberately.
    // It is a WELL-FORMED PEM that holds no private key, so the parser skips
    // the section it cannot use, finds nothing, and answers
    // `Err(NoItemsFound)` — which the mapping turns into the "held no
    // private key" refusal that is the one under test. Any other parse error
    // takes the `CredentialUnreadable` branch and never reaches it.
    let pem = minted();
    let failure = Joining::parse(
        CredentialFile {
            bytes: pem.leaf.as_bytes(),
            path: &at("leaf.pem"),
        },
        CredentialFile {
            bytes: pem.authority.as_bytes(),
            path: &at("key.pem"),
        },
        CredentialFile {
            bytes: pem.authority.as_bytes(),
            path: &at("ca.pem"),
        },
        DOOR.to_owned(),
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("a file with no key in it is not a key");
    let said = failure.to_string();
    assert!(said.contains("key.pem"), "names the file");
    assert!(said.contains("held no private key"), "refuses on content");
    assert!(
        !said.contains("BEGIN"),
        "a refusal about a key never quotes the file it read"
    );
}

#[test]
fn a_certificate_file_that_will_not_parse_is_refused_rather_than_read_as_empty() {
    // `certificates` is shared by the chain and the authority, and its error
    // branch had no test either: a file with no certificate in it and a file
    // whose certificate will not decode both ended at `chain.is_empty()`,
    // which reports the wrong one. A section that opens and will not decode
    // is unreadable, not absent.
    let pem = minted();
    let failure = Joining::parse(
        CredentialFile {
            bytes: b"-----BEGIN CERTIFICATE-----\n@@@@@@@@\n-----END CERTIFICATE-----\n",
            path: &at("leaf.pem"),
        },
        CredentialFile {
            bytes: pem.key.as_bytes(),
            path: &at("key.pem"),
        },
        CredentialFile {
            bytes: pem.authority.as_bytes(),
            path: &at("ca.pem"),
        },
        DOOR.to_owned(),
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("a certificate that will not decode is not a certificate");
    let said = failure.to_string();
    assert!(said.contains("leaf.pem"), "names the file");
    assert!(
        !said.contains("held no certificate"),
        "not the content refusal: the file held a section, it just would not read"
    );
}

#[test]
fn a_key_file_that_will_not_parse_is_a_different_refusal_and_still_quotes_nothing() {
    // The other half of the key mapping, and until W243 nothing exercised
    // it: every fixture fed the parser PEM that was absent rather than PEM
    // that was broken, so the two refusals were one tested branch and one
    // argument. A section that opens and then holds nothing decodable is a
    // file the parser could not READ, which is a different thing to tell an
    // operator than a file that held nothing of its kind.
    //
    // The body has to be outside the base64 alphabet to reach this branch,
    // and the first draft of this test did not know that. `not base64 at
    // all` is, letter for letter, valid base64, and this layer DECODES
    // rather than validates — so it parsed cheerfully into a `Pkcs8` key of
    // fifteen meaningless bytes and the test failed by succeeding. Whether a
    // key is a key is settled at the handshake, not here, and that was as
    // true of the parser this wave removed.
    let pem = minted();
    let failure = Joining::parse(
        CredentialFile {
            bytes: pem.leaf.as_bytes(),
            path: &at("leaf.pem"),
        },
        CredentialFile {
            bytes: b"-----BEGIN PRIVATE KEY-----\n@@@@@@@@\n-----END PRIVATE KEY-----\n",
            path: &at("key.pem"),
        },
        CredentialFile {
            bytes: pem.authority.as_bytes(),
            path: &at("ca.pem"),
        },
        DOOR.to_owned(),
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("a key that will not decode is not a key");
    let said = failure.to_string();
    assert!(said.contains("key.pem"), "names the file");
    assert!(
        !said.contains("held no private key"),
        "not the content refusal: the file held a section, it just would not read"
    );
    assert!(
        !said.contains("BEGIN") && !said.contains("@@@"),
        "a refusal about a key never quotes the file it read, however it failed"
    );
}

#[test]
fn an_authority_file_holding_two_certificates_is_refused_rather_than_half_trusted() {
    let pem = minted();
    let two = format!("{}{}", pem.authority, pem.authority);
    let failure = Joining::parse(
        CredentialFile {
            bytes: pem.leaf.as_bytes(),
            path: &at("leaf.pem"),
        },
        CredentialFile {
            bytes: pem.key.as_bytes(),
            path: &at("key.pem"),
        },
        CredentialFile {
            bytes: two.as_bytes(),
            path: &at("ca.pem"),
        },
        DOOR.to_owned(),
        vec![ONE_SEED.to_owned()],
    )
    .expect_err("the door trusts exactly one root");
    let said = failure.to_string();
    assert!(said.contains("2 certificates"), "says how many were found");
    assert!(said.contains("ca.pem"), "names the file");
}

#[test]
fn a_seed_is_the_node_and_the_address_together() {
    let seed = Seed::parse(ONE_SEED).expect("a well-formed seed");
    assert_eq!(seed.node, [0x1a; NODE_ID_LEN], "the id before the @");
    assert_eq!(seed.endpoint, "one.example:9080", "the address after it");
}

#[test]
fn a_seed_reads_the_hyphenated_form_of_an_id_too() {
    // `INFO FOR NODE` prints one form and a person copying an id out of a
    // ticket may paste the other. Both are the same sixteen bytes, and a
    // refusal that depends on which one was pasted would be a refusal about
    // punctuation dressed as a refusal about identity.
    let hyphenated = "1a1a1a1a-1a1a-1a1a-1a1a-1a1a1a1a1a1a@one.example:9080";
    let seed = Seed::parse(hyphenated).expect("the hyphenated form is an id");
    assert_eq!(seed.node, [0x1a; NODE_ID_LEN]);
}

#[test]
fn a_seed_that_is_only_an_address_is_refused_at_start() {
    // The form the flag carried until ADR-0067. It cannot be dialled — the
    // handshake derives the peer's name from its id — so it is refused here
    // rather than at the first round, where it would arrive as a TLS
    // failure and read like a certificate problem.
    let refused = Seed::parse("one.example:9080").expect_err("no id, no dial");
    let said = refused.to_string();
    assert!(said.contains("one.example:9080"), "quotes what was given");
    assert!(said.contains("no @"), "names which half is missing: {said}");
}

#[test]
fn a_seed_whose_id_is_not_an_id_is_refused_at_start() {
    let refused = Seed::parse("not-an-id@one.example:9080").expect_err("that is no id");
    assert!(
        refused.to_string().contains("not a node id"),
        "names the half that was wrong, not the whole value"
    );
}

#[test]
fn a_seed_with_an_id_and_no_address_is_refused_at_start() {
    let given = format!("{}@", "1a".repeat(NODE_ID_LEN));
    let refused = Seed::parse(&given).expect_err("nowhere to dial");
    assert!(
        refused.to_string().contains("no address after"),
        "an id on its own is not a seed"
    );
}

#[test]
fn a_path_that_does_not_exist_is_named_in_the_refusal() {
    let told = Told {
        chain: at("/nowhere/that/exists/leaf.pem"),
        key: at("/nowhere/that/exists/key.pem"),
        authority: at("/nowhere/that/exists/ca.pem"),
        door: DOOR.to_owned(),
        seeds: vec![ONE_SEED.to_owned()],
    };
    let failure = Joining::read(&told).expect_err("nothing to read");
    assert!(
        failure
            .to_string()
            .contains("/nowhere/that/exists/leaf.pem"),
        "names the path the operator gave"
    );
}
