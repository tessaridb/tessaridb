//! What can go wrong between a client and a node.
//!
//! Every variant answers a different question for whoever is looking at it: is
//! this the wrong program on the other end, the wrong version of the right one,
//! a client sending nonsense, or the store saying no? A single "protocol error"
//! would send somebody to read a packet capture for each of them.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure on the wire.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The socket failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A stored value could not be read back.
    #[error(transparent)]
    Encoding(#[from] tessari_encoding::Error),

    /// Whatever answered is not one of these.
    #[error("that is not a TessariDB node")]
    NotThisProtocol,

    /// It is one, of a version this build does not speak.
    ///
    /// Refused at the greeting rather than discovered mid-conversation, so a
    /// mismatch is one clear message at the start instead of a decode failure
    /// somewhere in the middle that reads like corruption.
    #[error("that node speaks version {found}; this build speaks {supported}")]
    WrongVersion {
        /// What it said.
        found: u8,
        /// What this build speaks.
        supported: u8,
    },

    /// A frame this build knows, arriving where a different one belongs.
    ///
    /// Kept apart from [`Error::UnknownFrame`] because the remedies are
    /// opposite: an unknown frame is a peer speaking a protocol this build does
    /// not have, while this one is a peer speaking the same protocol in the
    /// wrong order — a bug on the other side rather than a version to upgrade.
    #[error("frame kind {tag} arrived where the peer link expected a different one")]
    OutOfTurn {
        /// The tag that arrived.
        tag: u8,
    },

    /// A ballot naming a candidate other than the peer that presented it.
    ///
    /// The same family as [`Error::OutOfTurn`] and refused for the same reason:
    /// a peer speaking this protocol incorrectly, rather than a vote to answer.
    /// §C-07 settled that no node proxies, so there is no legitimate sender of
    /// somebody else's ballot — and a refusal would be the wrong shape anyway,
    /// because a `Vote` is an answer *about* a candidate and this frame has not
    /// established which candidate it is about.
    ///
    /// # Why this is load-bearing and was not before
    ///
    /// A voter now grants a ballot from the node it is **already holding a
    /// grant for**, even while that grant is alive, because re-granting to the
    /// holder adds no second holder. That makes the name in the ballot decide a
    /// grant. A peer free to write the incumbent's id into its own ballot would
    /// collect exactly the grants the liveness rule exists to withhold, and the
    /// cluster would have two holders — reached through the one door opened to
    /// let the incumbent keep the lease it already had.
    #[error("a ballot named a candidate other than the peer that proved itself")]
    NotItsOwnBallot,

    /// A collection asked from a position whose predecessor this node cannot
    /// state.
    ///
    /// A stream of records is only safe to apply if the receiver can be told
    /// which leadership wrote the record *before* the first one carried — that
    /// is what tells a re-send apart from a divergence, and without it a fork
    /// and a retry arrive looking identical. The epoch is read at `from - 1`,
    /// so a position this node holds nothing before cannot be served.
    ///
    /// # Two causes, one refusal, and that is deliberate
    ///
    /// A follower asking past the end of this node's log is either forked or
    /// mistaken; a follower asking from behind the retention floor needs a
    /// bootstrap rather than a collection. This node cannot tell which, and
    /// naming one would be a guess. What it must not do is answer *nothing*,
    /// because an empty collection is how a follower that is **level** is
    /// answered — so silence here would report a forked or stranded node as
    /// caught up.
    #[error(
        "this node cannot say what precedes sequence {from}, so it cannot be collected from there"
    )]
    Uncollectable {
        /// The position that was asked for.
        from: u64,
    },

    /// A peer asked for the log and nothing subscribed it.
    ///
    /// Distinct from [`Self::Uncollectable`] and from an empty collection,
    /// because the three have three different repairs: a grant, a re-bootstrap,
    /// and nothing at all. The message names the statement that grants one,
    /// since an operator reading this is holding a cluster where one node
    /// silently receives nothing.
    #[error(
        "no subscription is declared for this node — `DEFINE REPLICA <name> AT \
         '<endpoint>' NODE '<id>' REPLICATES <STORE|NAMESPACE …|DATABASE …>` on \
         the node that holds the original grants one"
    )]
    Unsubscribed,

    /// A frame kind this build does not have.
    ///
    /// The connection closes rather than the frame being skipped: a protocol
    /// that ignores what it does not understand is one where a version mismatch
    /// looks like silence.
    #[error("frame kind {tag} is not one this build knows")]
    UnknownFrame {
        /// The tag that arrived.
        tag: u8,
    },

    /// A declared length above what this build will read.
    ///
    /// Refused **before** the allocation, because a length from a stranger is
    /// not a promise — and a server that allocated whatever a client declared
    /// would be one packet away from being out of memory.
    #[error("a frame declared {length} bytes, which is more than this build will read")]
    TooLarge {
        /// What it declared.
        length: u32,
    },

    /// The stream ended inside something.
    #[error("the connection ended mid-frame")]
    Truncated,

    /// A body that does not hold what it claims.
    #[error("a frame's body is not the shape its own header says")]
    Malformed,

    /// This node may not take the write, and knows of no peer that may.
    ///
    /// The forward's target is missing rather than unreachable: nothing is
    /// declared `writable`, so there is no address to try. Said plainly and
    /// separately from a failed dial, because the two have different remedies —
    /// one is a `DEFINE REPLICA … ROLES writable` nobody ran, the other is a
    /// peer that is down.
    #[error("this node does not accept writes, and no peer is declared writable")]
    NoWritablePeer,

    /// The store said no, and this is what it said.
    ///
    /// Carried through verbatim rather than reworded: the session already writes
    /// messages that name the place in the script, and a wire layer improving on
    /// them would be a second author for one error.
    #[error("{message}")]
    Refused {
        /// The store's own words.
        message: String,
    },

    /// A peer connection arrived having proved nothing at all.
    ///
    /// Refused rather than defaulted: a handshake that admitted an unproven
    /// peer would make every check after it a formality performed on whoever
    /// asked. On a link that carries the whole log of every tenant, absence of
    /// proof is the one answer that must never be treated as an omission.
    #[error("that connection proved no identity, and this link admits no strangers")]
    Unidentified,

    /// A credential that was never issued for peering.
    ///
    /// Separate from a disagreeing id because the id may be perfectly correct:
    /// what went wrong is that a credential meant for one link was presented on
    /// another, which is a mis-issue or a copied file rather than an imposture.
    #[error("that credential was not issued for the peer link")]
    NotAPeerCredential,

    /// The id in the frame is not the id the credential names.
    ///
    /// Both are named, because whoever reads this needs to know which of the
    /// two is the node they configured. A node that could claim any id in a
    /// frame would make the credential decorative.
    #[error("that node's greeting claims {said} while its credential names {presented}")]
    IdentityDisagrees {
        /// What the frame said.
        said: String,
        /// What the transport proved.
        presented: String,
    },

    /// A peer arriving under **this node's own** id.
    ///
    /// The second of the two layers that make one id belong to one node. The
    /// first is issuance: this cluster signs a peer credential for a name only
    /// the node that generated the id can ask for, so an impostor has nothing
    /// to present. This is the door refusing it anyway, on the reasoning that a
    /// mis-issued credential is the case worth surviving — the layer that
    /// cannot be checked at run time is exactly the one worth not relying on
    /// alone.
    ///
    /// It is a different failure from every other refusal here and says so. A
    /// greeting that claims somebody else's id is a claim this node cannot
    /// adjudicate; a greeting that claims *this* node's id is one it can settle
    /// with certainty, because the other party to the collision is reading the
    /// frame.
    ///
    /// The consequence is not abstract. This node's own row is the one
    /// `Directory::greet_round` skips and `upstream` never picks, so a peer
    /// admitted under this id would be a peer nothing in the directory can ever
    /// choose to follow — present, greeted, and unreachable by construction.
    #[error("that node's greeting claims {said}, which is this node's own identity")]
    ClaimsOurOwnIdentity {
        /// The id both ends now name.
        said: String,
    },

    /// The transport itself refused, and this is what it said.
    ///
    /// A handshake that fails has already decided the connection is not
    /// happening, and the reasons are the transport's vocabulary rather than
    /// this protocol's — an untrusted issuer, an expired certificate, a name the
    /// server does not carry. Carried through as its own words for the same
    /// reason [`Self::Refused`] is: rewording somebody else's diagnosis gives an
    /// operator two accounts of one event.
    #[error("the peer link's transport refused this connection: {0}")]
    Transport(String),

    /// This node could not state what it holds, so it had nothing to greet with.
    ///
    /// Distinct from [`Self::Transport`] for the reason that one carries the
    /// transport's own words: the connection is fine and the peer is proved, and
    /// what failed is a read of this node's own store. Reporting it as a
    /// transport refusal would send an operator to the certificates when the
    /// answer is here.
    ///
    /// It exists because the greeting is read **when a peer arrives** rather
    /// than when the door opened, which is what makes the read able to fail
    /// inside the exchange at all.
    #[error("this node cannot say what it holds: {0}")]
    NothingToSay(String),

    /// A credential the transport accepted, which does not name this node.
    ///
    /// Distinct from [`Self::IdentityDisagrees`] by what it is able to say.
    /// That one is raised where the transport hands over an id it extracted, so
    /// both ids can be named. Here the credential was *asked* whether it speaks
    /// for the id the greeting claims and said no, and asking cannot report
    /// which id it would have said yes to. The fingerprint is the better half of
    /// that answer regardless: it names exactly one file on exactly one machine,
    /// which is what an operator chasing a mis-issued credential has to find.
    #[error(
        "that node's greeting claims {said}, and the credential it presented (sha256 {fingerprint}) does not name it"
    )]
    CredentialNamesAnother {
        /// What the frame said.
        said: String,
        /// The SHA-256 of the certificate that was presented, lowercase hex.
        fingerprint: String,
    },

    /// A seed was written in a form that cannot be dialled.
    ///
    /// A seed names a node and an address together, because the peer link
    /// verifies the certificate against `<node>.peer.tessari` and so a dial to a
    /// bare `host:port` is not expressible (ADR-0062, ADR-0067). Refused at
    /// start for the reason the credential read is: a typo found at the first
    /// dial is ten seconds away at best, and arrives looking like a network
    /// fault rather than like a flag somebody mistyped.
    #[error("--seed {given:?} is not <node-id>@<host:port>: {reason}")]
    SeedMalformed {
        /// Exactly what the flag was given, so the operator can see the typo.
        given: String,
        /// Which half of the form was wrong.
        reason: &'static str,
    },

    /// A node told some of what a cluster takes, and not the rest.
    ///
    /// Refused at start rather than carried, for the reason
    /// `bootstrap::first_user` already refuses a misconfigured credential: a node
    /// that came up because nobody checked is a node answering on a network in a
    /// state its operator did not ask for. Half a cluster configuration is that
    /// same failure — it would leave a node believing it has peers it cannot
    /// reach, or cannot prove itself to, while looking exactly like one that
    /// started correctly.
    #[error("this node was told {given} but not {missing}; half a cluster is not a configuration")]
    ClusterHalfConfigured {
        /// What was supplied, in the order the parts are named.
        given: String,
        /// What was left out.
        missing: String,
    },

    /// A file a cluster configuration named could not be read.
    ///
    /// Names the path and the operating system's own words, and never the
    /// contents — one of these files is a private key, and a refusal that quotes
    /// what it failed to parse is a refusal that puts the key in a log.
    #[error("{part} could not be read from {path}: {reason}")]
    CredentialUnreadable {
        /// Which of the parts this was.
        part: &'static str,
        /// The path as the operator gave it.
        path: String,
        /// What the operating system said.
        reason: String,
    },

    /// A file was read, and held nothing of the kind it was named for.
    ///
    /// Distinct from [`Self::CredentialUnreadable`]: the file exists and was
    /// readable, so the operator is looking for a content problem rather than a
    /// path problem, and saying "not found" would send them to the wrong one.
    #[error("{part} at {path} held no {wanted}")]
    CredentialEmpty {
        /// Which of the parts this was.
        part: &'static str,
        /// The path as the operator gave it.
        path: String,
        /// What was looked for and not found.
        wanted: &'static str,
    },

    /// An authority file holding some number of certificates other than one.
    ///
    /// `Peers::bind` trusts **exactly one** root, so a file carrying two would
    /// have one of them silently ignored — and the ignored one is as likely to
    /// be the new authority during a rotation as the old. A trust gap that
    /// reports success is worth refusing over.
    #[error(
        "the cluster authority at {path} holds {found} certificates; a cluster is issued by exactly one"
    )]
    AuthorityNotSingle {
        /// The path as the operator gave it.
        path: String,
        /// How many were found.
        found: usize,
    },

    /// A role set carrying a bit this build does not assign.
    ///
    /// Not a malformed frame — the body is exactly the shape a greeting takes.
    /// It carries a fact from a build that knows more than this one, and
    /// reporting that as corruption would send somebody to the wrong question.
    #[error("that node holds role bits {bits:#010b}, which this build does not know")]
    UnknownRoles {
        /// The bits that arrived.
        bits: u8,
    },

    /// A redirect decided under a leadership this caller has already moved past.
    ///
    /// Raft §3.3's shape and its reason: a party meeting a term below its own
    /// rejects and returns its own, so the sender learns in the same round trip
    /// that it is behind. Here the epoch plays the term's role.
    ///
    /// **Why refusing is better than following it.** Undated, a caller walking a
    /// chain of redirects cannot tell a LOOP from PROGRESS — it arrives, is sent
    /// on, arrives, is sent on. Dated, a redirect naming an epoch already
    /// superseded is recognisably a replay of a decision that no longer holds: a
    /// node with a stale directory, a frame that took a long path, a copy
    /// somebody kept. Following it walks back into the arrangement that has
    /// already been left.
    ///
    /// **Equal is not stale.** Two redirects under one leadership are ordinary —
    /// a caller sent to one node, and that node sending it to a second, both
    /// deciding under the same epoch. Only strictly older is refused.
    #[error(
        "that redirect was decided under leadership {named}, and this caller has \
         already been told about {held}: it names an arrangement that has been \
         superseded"
    )]
    StaleRedirect {
        /// The leadership the redirect named.
        named: tessari_types::Epoch,
        /// The newest leadership this caller has been told about.
        held: tessari_types::Epoch,
    },

    /// A read this node declined, answered by a caller that cannot follow it.
    ///
    /// Not what the redirect IS — it is an instruction and
    /// [`crate::Client::run_routed`] hands it over intact. This is what becomes
    /// of one when the caller asked for answers and has no way to act on being
    /// sent elsewhere: a genuine failure, and one that says where the read
    /// belonged rather than reporting an unknown frame.
    #[error("that read belongs at {endpoint}, which this caller cannot follow")]
    Redirected {
        /// The address the read belonged at.
        endpoint: String,
        /// Who was expected there.
        node: [u8; tessari_encoding::NODE_ID_LEN],
    },

    /// A redirect naming a settlement this build does not assign.
    ///
    /// The same distinction [`Self::UnknownRoles`] draws, for the same reason:
    /// the body is exactly the shape a redirect takes, so this is a newer build
    /// on the other end rather than a broken frame, and reporting it as
    /// corruption would send somebody to the wrong question.
    #[error("that redirect carries settlement {byte}, which this build does not know")]
    UnknownSettlement {
        /// The byte that arrived.
        byte: u8,
    },
}
