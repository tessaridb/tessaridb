use super::{FailoverStamp, Hello, Line, PeerFrame, Presented, Purpose, admit};
use crate::error::Error;
use crate::frame;
use core::time::Duration;
use tessari_encoding::{NODE_ID_LEN, NodeIdentity, NodeVersion, Roles};
use tessari_types::{DatabaseId, Epoch, NamespaceId, Reach, Sequence, ShardId, TableId};

const ONE: [u8; NODE_ID_LEN] = [1; NODE_ID_LEN];
const ANOTHER: [u8; NODE_ID_LEN] = [2; NODE_ID_LEN];
/// The node doing the admitting, distinct from both peers above so that the
/// fourth refusal cannot fire by accident in a test about the other three.
const ME: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

fn greeting(node: [u8; NODE_ID_LEN]) -> Hello {
    Hello {
        node,
        build: NodeVersion {
            major: 0,
            minor: 1,
            patch: 1,
        },
        epoch: Epoch::new(7),
        roles: Roles::ALONE,
        tail: Sequence::new(4096),
        tail_leadership: Epoch::new(7),
        current_as_of: Some(Duration::from_secs(3)),
        policy: None,
        line: None,
    }
}

#[test]
fn no_peer_tag_is_a_client_tag() {
    // The whole reason the peer protocol gets its own enum: the two spaces
    // must not overlap, and a comment saying so is not a check.
    for tag in 0..=u8::MAX {
        let peer = PeerFrame::from_tag(tag).is_some();
        let client = frame::Kind::from_tag(tag).is_some();
        assert!(
            !(peer && client),
            "tag {tag} is claimed by both the peer and the client protocol"
        );
    }
    // The safety property stated directly rather than inferred from the
    // sweep: every peer tag must be one the client's reader refuses, because
    // that refusal is what closes a misdirected connection instead of
    // decoding it as something else.
    for kind in [
        PeerFrame::Hello,
        PeerFrame::Ballot,
        PeerFrame::Vote,
        PeerFrame::Collect,
        PeerFrame::Collected,
        PeerFrame::Uncollectable,
        PeerFrame::Unsubscribed,
        PeerFrame::Gather,
        PeerFrame::Gathered,
        PeerFrame::NotGathered,
        PeerFrame::State,
        PeerFrame::StateHead,
        PeerFrame::StateChunk,
        PeerFrame::StateEnd,
        PeerFrame::Stream,
        PeerFrame::Streamed,
        PeerFrame::StreamFrom,
        PeerFrame::Held,
        PeerFrame::Restarted,
    ] {
        assert!(
            frame::Kind::from_tag(kind.tag()).is_none(),
            "the client reader accepts {kind:?}, which it must close on"
        );
    }
}

/// G033 S2.2 — the seven tags a peer of an earlier build reads keep their
/// numbers, and the three gather frames take the next free ones.
#[test]
fn every_peer_tag_keeps_its_number() {
    let expected = [
        (PeerFrame::Hello, 6),
        (PeerFrame::Ballot, 7),
        (PeerFrame::Vote, 8),
        (PeerFrame::Collect, 9),
        (PeerFrame::Collected, 10),
        (PeerFrame::Uncollectable, 11),
        (PeerFrame::Unsubscribed, 12),
        (PeerFrame::Gather, 14),
        (PeerFrame::Gathered, 15),
        (PeerFrame::NotGathered, 16),
        (PeerFrame::State, 18),
        (PeerFrame::StateHead, 19),
        (PeerFrame::StateChunk, 20),
        (PeerFrame::StateEnd, 21),
        (PeerFrame::Stream, 22),
        (PeerFrame::Streamed, 23),
        (PeerFrame::Coordinate, 24),
        (PeerFrame::Coordinated, 25),
        (PeerFrame::NotCoordinated, 26),
        (PeerFrame::Attempt, 27),
        (PeerFrame::Attempted, 28),
        (PeerFrame::Join, 29),
        (PeerFrame::Joined, 30),
        (PeerFrame::Across, 31),
        (PeerFrame::AcrossDone, 32),
        (PeerFrame::NotAcross, 33),
        (PeerFrame::StreamFrom, 34),
        (PeerFrame::Held, 35),
        (PeerFrame::Restarted, 36),
    ];
    for (kind, tag) in expected {
        assert_eq!(kind.tag(), tag, "{kind:?}");
        assert_eq!(PeerFrame::from_tag(tag), Some(kind));
    }
    assert_eq!(
        (0..=u8::MAX)
            .filter(|tag| PeerFrame::from_tag(*tag).is_some())
            .count(),
        expected.len()
    );
}

#[test]
fn a_greeting_survives_the_wire_unchanged() {
    let said = greeting(ONE);
    let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
    assert_eq!(heard, said);
}

#[test]
fn a_greeting_carries_how_old_its_own_copy_is() {
    // The fact a router needs, and the one a greeting did not carry until
    // this wave. Three readings, because they are three different claims: a
    // copy of a known age, a copy whose age nobody can state, and a fraction
    // of a second -- which must come back as the whole second ABOVE it, so
    // the rounding can only refuse a borderline read and never admit one.
    let mut said = greeting(ONE);

    said.current_as_of = Some(Duration::from_secs(41));
    let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
    assert_eq!(heard.current_as_of, Some(Duration::from_secs(41)));

    said.current_as_of = None;
    let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
    assert_eq!(
        heard.current_as_of, None,
        "a node that cannot say how old its copy is came back claiming an age"
    );

    said.current_as_of = Some(Duration::from_millis(1_500));
    let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
    assert_eq!(
        heard.current_as_of,
        Some(Duration::from_secs(2)),
        "a fraction of a second rounded the copy younger, which is the one \
             direction a staleness reading may never move"
    );
}

/// The policy stamp's width on the wire: a presence byte and two `u64`s.
const POLICY_BYTES: usize = 17;

/// Where a greeting written before the policy stamp existed ends.
///
/// Derived from the encoding rather than written as a number, so that a
/// later field appended after this one moves the boundary instead of
/// silently making this test assert the wrong offset. It is the boundary the
/// compatibility rule is about: a body that ENDS here is a peer built before
/// the field, and a body that stops anywhere else is a truncation.
fn before_the_policy(whole: &[u8]) -> usize {
    whole.len().saturating_sub(POLICY_BYTES)
}

#[test]
fn a_greeting_that_stops_early_is_malformed_rather_than_a_panic() {
    let whole = greeting(ONE).encode();
    for stop in 0..whole.len() {
        // The one prefix that is not a truncation: a greeting from a build
        // that predates the policy stamp ends exactly here, and it has its
        // own test below.
        if stop == before_the_policy(&whole) {
            continue;
        }
        let cut = whole.get(..stop).expect("a prefix of a vector");
        assert!(
            matches!(Hello::decode(cut), Err(Error::Malformed)),
            "{stop} bytes of a greeting decoded as something other than malformed"
        );
    }
}

#[test]
fn a_greeting_carries_which_failover_policy_its_node_runs_under() {
    // Two readings, because they are two different claims: a node that has
    // been told which policy the cluster runs under, and a node that has
    // not. The second is the ordinary state of a cluster nobody has
    // configured and must survive the wire as `None` rather than as a pair
    // of zeroes, which would be a policy set under the first leadership.
    let mut said = greeting(ONE);

    said.policy = Some(FailoverStamp {
        epoch: Epoch::new(4),
        version: 2,
    });
    let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
    assert_eq!(
        heard.policy,
        Some(FailoverStamp {
            epoch: Epoch::new(4),
            version: 2
        })
    );

    said.policy = None;
    let heard = Hello::decode(&said.encode()).expect("a greeting this build wrote");
    assert_eq!(
        heard.policy, None,
        "a node running no declared policy came back claiming one"
    );
}

#[test]
fn a_greeting_from_a_build_without_the_policy_field_is_a_node_with_nothing_to_say() {
    // The compatibility rule, asserted rather than asserted-in-a-comment: a
    // peer built before the field ends its body where the field would have
    // started. That is a node saying nothing about a policy, and it must
    // never read as a broken greeting -- a rolling upgrade in which half the
    // cluster refuses the other half's greetings is an outage produced by
    // adding a field nobody needed yet.
    let whole = greeting(ONE).encode();
    let older = whole
        .get(..before_the_policy(&whole))
        .expect("a greeting without its last field");
    let heard = Hello::decode(older).expect("an older peer's greeting is not malformed");
    assert_eq!(heard.policy, None);
    // And the rest of it survived: a tolerant tail must not become a
    // tolerant reader, or a genuinely short body would decode as a greeting
    // full of defaults.
    assert_eq!(heard.node, ONE);
    assert_eq!(heard.tail, Sequence::new(4096));
    assert_eq!(heard.current_as_of, Some(Duration::from_secs(3)));
}

#[test]
fn a_policy_field_that_half_arrived_is_refused() {
    // The other side of the tolerance, and the reason it is a boundary
    // rather than a range: a body that STARTS the field and then stops is a
    // truncation, not an older build, and guessing the rest of it would be
    // inventing a cluster-wide ordering out of missing bytes.
    let whole = greeting(ONE).encode();
    for stop in before_the_policy(&whole).saturating_add(1)..whole.len() {
        let cut = whole.get(..stop).expect("a prefix of a vector");
        assert!(
            matches!(Hello::decode(cut), Err(Error::Malformed)),
            "{stop} bytes -- a half-written policy stamp decoded as a greeting"
        );
    }
}

#[test]
fn a_role_this_build_does_not_know_is_a_newer_peer_and_says_so() {
    // Not `Malformed`: the body is exactly the shape a greeting takes. What
    // it carries is a fact from a build that knows more than this one, and
    // reporting that as a broken frame would send somebody to the wrong
    // question.
    let mut body = greeting(ONE).encode();
    let at = NODE_ID_LEN.saturating_add(20);
    *body
        .get_mut(at)
        .expect("the role byte a greeting always carries") = 0b1000_0000;
    assert!(matches!(
        Hello::decode(&body),
        Err(Error::UnknownRoles { bits: 0b1000_0000 })
    ));
}

#[test]
fn a_node_whose_frame_agrees_with_its_credential_is_admitted() {
    let presented = Presented {
        node: ONE,
        purpose: Purpose::Peer,
    };
    assert!(admit(Some(&presented), &greeting(ONE), &ME).is_ok());
}

#[test]
fn a_connection_that_proved_nothing_is_refused_before_anything_else() {
    // Absence is the default-deny case, and it is checked first: a greeting
    // from nobody is not improved by being well formed.
    assert!(matches!(
        admit(None, &greeting(ONE), &ME),
        Err(Error::Unidentified)
    ));
}

#[test]
fn a_clients_credential_on_the_peer_link_is_refused_on_its_purpose() {
    // The id here is perfectly correct, which is the point: this refusal is
    // about what the credential was issued for and nothing else.
    let presented = Presented {
        node: ONE,
        purpose: Purpose::Client,
    };
    assert!(matches!(
        admit(Some(&presented), &greeting(ONE), &ME),
        Err(Error::NotAPeerCredential)
    ));
}

#[test]
fn a_peer_arriving_under_this_nodes_own_id_is_refused() {
    // The credential agrees with the frame, which is what makes this worth
    // checking: every other refusal has already passed, so the only thing
    // left that can catch it is this end knowing its own name. A cluster
    // that issued this credential mis-issued it, and the door survives that
    // rather than trusting it did not happen.
    let presented = Presented {
        node: ME,
        purpose: Purpose::Peer,
    };
    let refused = admit(Some(&presented), &greeting(ME), &ME)
        .expect_err("a peer claiming this node's own id was admitted");
    assert!(matches!(refused, Error::ClaimsOurOwnIdentity { .. }));
    let said = refused.to_string();
    assert!(
        said.contains(&"03".repeat(NODE_ID_LEN)),
        "the claim: {said}"
    );
}

#[test]
fn an_unproven_connection_claiming_this_nodes_id_is_refused_for_proving_nothing() {
    // The order of the two checks stated as a property rather than left to
    // the reading. A caller with no credential gets the refusal that names
    // the real problem; being told it collided with an identity would send
    // whoever reads the log looking for a second node that does not exist.
    assert!(matches!(
        admit(None, &greeting(ME), &ME),
        Err(Error::Unidentified)
    ));
}

#[test]
fn a_frame_claiming_an_id_the_credential_does_not_name_is_refused() {
    let presented = Presented {
        node: ONE,
        purpose: Purpose::Peer,
    };
    let refused = admit(Some(&presented), &greeting(ANOTHER), &ME)
        .expect_err("a frame naming another node was admitted");
    assert!(matches!(refused, Error::IdentityDisagrees { .. }));
    // Both ids reach the operator, because whoever reads this needs to know
    // which of the two is the node they configured. Asserted on the rendered
    // message rather than on the fields: the rendering is what they see.
    let said = refused.to_string();
    assert!(
        said.contains(&"02".repeat(NODE_ID_LEN)),
        "the claim: {said}"
    );
    assert!(
        said.contains(&"01".repeat(NODE_ID_LEN)),
        "the credential: {said}"
    );
}

#[test]
fn a_node_greets_under_the_identity_it_actually_holds() {
    // The reason `about` exists: a greeting assembled field by field could
    // disagree with the node's own stored identity, and nothing downstream
    // would ever see the difference.
    let identity = NodeIdentity::alone(ONE);
    let said = Hello::about(
        &identity,
        Epoch::new(3),
        Sequence::new(90),
        Epoch::new(2),
        Some(Duration::from_secs(11)),
        None,
    );
    assert_eq!(said.node, identity.id);
    assert_eq!(said.roles, identity.roles);
    assert_eq!(said.build, identity.version);
}

// ---- G032 S3.1: the placed line on the wire ------------------------------

/// The one placed line the greeting tests carry.
const PLACED: Line = Line {
    range: Reach::Shard(
        NamespaceId::new(1),
        DatabaseId::new(2),
        TableId::new(3),
        ShardId::new(2),
    ),
    leading: Epoch::new(5),
    tail: Sequence::new(12),
    tail_leadership: Epoch::new(4),
};

#[test]
fn a_greeting_with_no_placed_line_keeps_the_bytes_it_always_had() {
    // The kill criterion (G032). Written from `Hello::encode` as it stood at
    // `a1d0025`, piece by piece, rather than from the encoder under test.
    let mut golden = Vec::new();
    golden.extend_from_slice(&ONE);
    golden.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1]);
    golden.extend_from_slice(&7_u64.to_be_bytes());
    golden.push(Roles::ALONE.bits());
    golden.extend_from_slice(&4096_u64.to_be_bytes());
    golden.extend_from_slice(&7_u64.to_be_bytes());
    golden.push(1);
    golden.extend_from_slice(&3_u64.to_be_bytes());
    golden.push(0);
    golden.extend_from_slice(&[0; 16]);
    assert_eq!(golden.len(), 79);
    assert_eq!(greeting(ONE).encode(), golden);
}

#[test]
fn a_greeting_carries_its_placed_line_and_one_without_it_reads_as_none() {
    let mut said = greeting(ONE);
    said.line = Some(PLACED);
    let whole = said.encode();
    let heard = Hello::decode(&whole).expect("a greeting this build wrote");
    assert_eq!(heard.line, Some(PLACED));
    assert_eq!(heard, said);
    // The body without the line is a node with no placement, never a
    // truncation.
    let older = whole.get(..79).expect("the placement-free prefix");
    assert_eq!(Hello::decode(older).expect("an older greeting").line, None);
    // And a line that half-arrived is refused rather than guessed.
    for stop in 80..whole.len() {
        let cut = whole.get(..stop).expect("a prefix of a vector");
        assert!(
            matches!(Hello::decode(cut), Err(Error::Malformed)),
            "{stop} bytes -- a half-written line decoded as a greeting"
        );
    }
}

#[test]
fn a_greeting_answers_its_position_on_a_range_it_stands_for_and_zero_elsewhere() {
    let mut said = greeting(ONE);
    said.line = Some(PLACED);
    assert_eq!(said.reached_on(PLACED.range), PLACED.reached());
    assert_eq!(said.reached_on(Reach::Store), said.reached());
    let other = Reach::Namespace(NamespaceId::new(9));
    assert_eq!(said.reached_on(other).tail, Sequence::ZERO);
    assert_eq!(said.reached_on(other).leadership, Epoch::ZERO);
}
