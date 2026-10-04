//! Reading the command line.
//!
//! # An unknown option is an error rather than something ignored
//!
//! A session opened with a misspelled flag that silently used the default is how
//! somebody writes to the wrong store. The same reasoning runs through the rest
//! of this module: two stores are refused rather than ordered, and asking for a
//! store operation over an address is refused rather than quietly run against a
//! store in this process.

use std::env;
use std::path::PathBuf;

use tessaridb::{Parameters, Value};

pub const USAGE: &str = "\
usage: tessaridb [<path> | --at <host:port>] [-e <script> | -f <file>]

  <path>          a store on disk; omitted, the store is in memory and is lost
  --at <host:port> a running node, instead of a store in this process
  --user <name>   sign in as this user; the password comes from TESSARIDB_PASSWORD,
                  never from an argument, which the process table would publish
  --serve <host:port> serve this store over the wire protocol until stopped
  --http <host:port> serve this store over HTTP until stopped; may accompany
                  --serve, and one process then holds both
  --backup-dir <folder> where a serving node writes `BACKUP … TO '<name>'`;
                  without it every `TO` is refused
  --unseal-for <duration> how long an unseal lasts before the store seals itself,
                  written as TessariQL (`10m`, `1h`); default TESSARIDB_UNSEAL_FOR
                  or 10m
  --tls-cert <file> serve clients over TLS with this certificate chain, PEM;
                  default TESSARIDB_TLS_CERT
  --tls-key <file> its private key, PEM; default TESSARIDB_TLS_KEY
  --require-client-tls refuse to start unless --tls-cert and --tls-key are
                  given, for a deployment that forbids the clear; or
                  TESSARIDB_REQUIRE_CLIENT_TLS=1; off by default
  --client-plaintext refused since 0.24.0-beta: the clear is the default without
                  a certificate, so the flag is dropped rather than given
  --tls-authority <file> with --at: speak TLS and trust the node by these
                  certificates, PEM; default TESSARIDB_TLS_AUTHORITY
  --cluster-credential <file> this node's peer credential, PEM, with --serve
  --cluster-key <file> its private key, PEM
  --cluster-authority <file> the one certificate this cluster trusts, PEM
  --cluster-address <host:port> where this node's own peer door binds
  --seed <node-id>@<host:port> a node to reach the cluster through; repeatable
  --join-token <token> a token from CREATE JOIN TOKEN, offered to the seeds until
                  a row names this node; default TESSARIDB_JOIN_TOKEN
  --encryption-key-file <file> the store's files and every backup it writes are
                  encrypted under the 32 bytes in <file> (`openssl rand 32`,
                  mode 600); a store opens only the way it was created; default
                  TESSARIDB_ENCRYPTION_KEY_FILE
  --backup-key-file <file> with --restore or --verify: the key a sealed backup
                  was sealed under, when it is not the store's — how a store
                  moves to a new key
  --param <name>=<value> bind $name to <value>, written as TessariQL; repeatable
  -e, --execute <script> run this and exit
  -f, --file <file> run this file and exit
  --backup <file> write the store's current state (a snapshot) to <file> and
                  exit; with --from, its log instead
  --snapshot <file> write the store's current state to <file> and exit
  --dump <file>   write the store's current state as TessariQL to <file> and exit
  --verify <file> read <file> and say what it holds, changing nothing
  --from <n>      with --backup: write the log from <n> (1 for the whole log)
  --upto <n>      with --restore: stop replaying after sequence <n>
  --restore <file> replay <file> into an empty store and exit
  --health        say whether the store is well, and exit non-zero if not
  -V, --version   say which build this is, and exit
  -h, --help      this

a node given --tls-cert and --tls-key speaks TLS on every client surface — the
wire port, HTTP and the WebSocket on it — and nothing else. Without one a node,
single or clustered, serves its clients in the clear and says so when it starts;
--require-client-tls makes it refuse to start instead. The peer link is mutual
TLS whatever the clients were given.

the five cluster options are given together or not at all: told some of them a
node refuses to start rather than serving with credentials nobody checked, and
told none of them it is the single node it is today. there is no default peer
address, because there is no port this engine claims and a defaulted listener is
a door the operator did not know they opened.

with neither -e nor -f, statements are read from standard input: a prompt when
that is a terminal, a script when it is a pipe.

serving a store that has no users yet, TESSARIDB_INITIAL_USER and
TESSARIDB_INITIAL_PASSWORD declare that user as a store-wide owner and close the
store. Both or neither: half of them is refused rather than started, because a
node that came up open because a variable was misspelled looks exactly like one
that came up correctly. A store that already has users ignores them, so a
container may carry them on every restart, and they are not a way to reset a
password.

a serving node keeps the newest 100000 records of each log and prunes the rest;
TESSARIDB_RETAIN_RECORDS sets another count, or `none` for every record, and
`DEFINE NODE RETAIN` stored on the node wins over both.";

/// Where the password is read from.
///
/// Not an argument. An argument is in the process table for anybody on the
/// machine to read and in the shell history afterwards, which is a defect rather
/// than the convenience it looks like.
pub const PASSWORD: &str = "TESSARIDB_PASSWORD";

/// What the command line asked for.
#[derive(Debug)]
pub struct Asked {
    /// The store on disk, when one was named.
    pub store: Option<PathBuf>,
    /// The node to talk to instead.
    pub at: Option<String>,
    /// Who to sign in as.
    pub user: Option<String>,
    /// Where the statements come from.
    pub source: Source,
    /// The values the script's parameters bind to.
    pub parameters: Parameters,
    /// The sequence a backup starts at, or a restore stops after.
    ///
    /// One field for two flags because they are the same kind of thing — a
    /// position in the log — and which one it means is decided by the source it
    /// accompanies, which the parser has already refused to make ambiguous.
    pub at_sequence: Option<u64>,
    /// The addresses to serve on, when `Source::Serve` was asked for.
    pub serving: Serving,
    /// The certificates `--at` trusts a node by; given, the client speaks TLS.
    pub authority: Option<PathBuf>,
    /// The file holding the key the store and its backups are encrypted under
    /// (ADR-0108 D7), when the flag named one.
    pub encryption_key: Option<PathBuf>,
    /// The key a sealed backup opens under, when it is not the store's.
    pub backup_key: Option<PathBuf>,
    /// The cluster this node was told to join, when it was told about one.
    ///
    /// `None` is the single node every current deployment is, and is not a
    /// defect: see `tessari_wire::Told`, which owns the rule that decides
    /// between *told nothing* and *told half*.
    pub cluster: Option<tessari_wire::Told>,
}

/// Where a serving process listens.
///
/// One field per surface rather than one address, because a process holds every
/// surface and any subset of them may be absent. The previous shape — a single
/// address carried by the source — is what made two surfaces unaskable: not the
/// implementation, the grammar.
#[derive(Debug, Default)]
pub struct Serving {
    /// The wire protocol: framed TCP carrying values in the store's own codec.
    pub wire: Option<String>,
    /// HTTP.
    pub http: Option<String>,
    /// The folder `BACKUP … TO` writes into; absent, every `TO` is refused.
    pub backups: Option<PathBuf>,
    /// How long an unseal lasts; absent, `TESSARIDB_UNSEAL_FOR` or ten minutes.
    pub unseal_for: Option<core::time::Duration>,
    /// What the TLS flags said; the environment fills the rest at start-up.
    pub tls: crate::tls::Given,
    /// A join token to offer the seeds (ADR-0108 D9); the environment fills
    /// it at start-up when absent.
    pub join_token: Option<String>,
}

impl Serving {
    /// Whether any surface was asked for.
    #[must_use]
    pub const fn asked(&self) -> bool {
        self.wire.is_some() || self.http.is_some()
    }
}

/// Where the statements come from, or what else was asked for.
#[derive(Debug)]
pub enum Source {
    /// Standard input, prompting or not depending on what it is.
    Standard,
    /// One script given on the command line.
    Inline(String),
    /// A file.
    File(PathBuf),
    /// Write this store's log to a file.
    Backup(PathBuf),
    /// Write this store's current state to a file (ADR-0091).
    Snapshot(PathBuf),
    /// Write this store's current state as a TessariQL script (ADR-0091).
    Dump(PathBuf),
    /// Replay a file into this store.
    Restore(PathBuf),
    /// Say whether the store is well.
    Health,
    /// Read a backup and say what it holds, without applying any of it.
    ///
    /// Needs no store, which is the point: a backup that can only be checked by
    /// restoring it is a backup nobody checks.
    Verify(PathBuf),
    /// Serve this store, on whichever surfaces `Asked::serving` names.
    Serve,
    /// Say which build this is, and nothing else.
    ///
    /// Needs no store, like `Verify`, and for a stronger reason: the first
    /// thing anybody does with a binary they have just been handed is ask it
    /// what it is, and a version that could only be obtained by opening a store
    /// would be unavailable at exactly that moment.
    Version,
    /// Print the usage and stop.
    ///
    /// A request rather than a refusal, which is the whole reason it is a
    /// variant instead of an early `Err`: asking a program for its help is not
    /// an error, and answering on standard error with a non-zero status breaks
    /// `tessaridb --help | grep serve` and fails any packaging smoke test that
    /// runs it.
    Help,
}

/// Read the arguments, refusing anything unrecognised.
///
/// An unknown option is an error rather than something ignored: a session opened
/// with a misspelled flag that silently used the default is how somebody writes
/// to the wrong store.
pub fn parse(arguments: impl Iterator<Item = String>) -> Result<Asked, String> {
    let mut store = None;
    let mut at = None;
    let mut user = None;
    let mut source = Source::Standard;
    let mut serving = Serving::default();
    let mut parameters = Parameters::new();
    let mut sequence = None;
    let mut credential = None;
    let mut key = None;
    let mut authority = None;
    let mut door = None;
    let mut seeds: Vec<String> = Vec::new();
    let mut trusted = None;
    let mut encryption_key = None;
    let mut backup_key = None;
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => source = Source::Help,
            // Read here rather than short-circuited before parsing, so that
            // `--version` alongside a misspelled flag still complains about the
            // misspelling. This module refuses unrecognised options everywhere
            // else and an exception would be one place a typo goes unreported.
            "--version" | "-V" => source = Source::Version,
            "-e" | "--execute" => {
                let script = arguments
                    .next()
                    .ok_or_else(|| "-e wants a script".to_owned())?;
                source = Source::Inline(script);
            }
            "-f" | "--file" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "-f wants a path".to_owned())?;
                source = Source::File(PathBuf::from(path));
            }
            "--backup" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--backup wants a path".to_owned())?;
                source = Source::Backup(PathBuf::from(path));
            }
            "--snapshot" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--snapshot wants a path".to_owned())?;
                source = Source::Snapshot(PathBuf::from(path));
            }
            "--dump" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--dump wants a path".to_owned())?;
                source = Source::Dump(PathBuf::from(path));
            }
            "--health" => source = Source::Health,
            "--verify" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--verify wants a path".to_owned())?;
                source = Source::Verify(PathBuf::from(path));
            }
            "--from" | "--upto" => {
                let written = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} wants a sequence"))?;
                sequence = Some(
                    written
                        .parse::<u64>()
                        .map_err(|_| format!("{argument} wants a sequence, not {written:?}"))?,
                );
            }
            "--at" => {
                let address = arguments
                    .next()
                    .ok_or_else(|| "--at wants a host:port".to_owned())?;
                at = Some(address);
            }
            "--param" => {
                let given = arguments
                    .next()
                    .ok_or_else(|| "--param wants <name>=<value>".to_owned())?;
                let (name, value) = parameter(&given)?;
                parameters.insert(name, value);
            }
            "--user" => {
                let name = arguments
                    .next()
                    .ok_or_else(|| "--user wants a name".to_owned())?;
                user = Some(name);
            }
            "--serve" => {
                let address = arguments
                    .next()
                    .ok_or_else(|| "--serve wants a host:port".to_owned())?;
                serving.wire = Some(address);
                source = Source::Serve;
            }
            "--http" => {
                let address = arguments
                    .next()
                    .ok_or_else(|| "--http wants a host:port".to_owned())?;
                serving.http = Some(address);
                source = Source::Serve;
            }
            "--backup-dir" => {
                let folder = arguments
                    .next()
                    .ok_or_else(|| "--backup-dir wants a folder".to_owned())?;
                serving.backups = Some(PathBuf::from(folder));
            }
            "--unseal-for" => {
                let written = arguments
                    .next()
                    .ok_or_else(|| "--unseal-for wants a duration, such as 10m".to_owned())?;
                serving.unseal_for =
                    Some(unseal_period(&written).map_err(|why| format!("--unseal-for {why}"))?);
            }
            "--tls-cert" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--tls-cert wants a path".to_owned())?;
                serving.tls.cert = Some(PathBuf::from(path));
            }
            "--tls-key" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--tls-key wants a path".to_owned())?;
                serving.tls.key = Some(PathBuf::from(path));
            }
            "--client-plaintext" => {
                return Err(format!(
                    "--client-plaintext {}",
                    crate::tls::PLAINTEXT_RETIRED
                ));
            }
            "--require-client-tls" => serving.tls.require = true,
            "--join-token" => {
                serving.join_token = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--join-token wants the token".to_owned())?,
                );
            }
            "--encryption-key-file" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--encryption-key-file wants a path".to_owned())?;
                encryption_key = Some(PathBuf::from(path));
            }
            "--backup-key-file" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--backup-key-file wants a path".to_owned())?;
                backup_key = Some(PathBuf::from(path));
            }
            "--tls-authority" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--tls-authority wants a path".to_owned())?;
                trusted = Some(PathBuf::from(path));
            }
            "--cluster-credential" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--cluster-credential wants a path".to_owned())?;
                credential = Some(PathBuf::from(path));
            }
            "--cluster-key" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--cluster-key wants a path".to_owned())?;
                key = Some(PathBuf::from(path));
            }
            "--cluster-authority" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--cluster-authority wants a path".to_owned())?;
                authority = Some(PathBuf::from(path));
            }
            "--cluster-address" => {
                door = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--cluster-address wants a host:port".to_owned())?,
                );
            }
            // Repeatable, because one seed is one point of failure at the
            // moment a cluster is least able to afford one.
            "--seed" => {
                seeds.push(
                    arguments
                        .next()
                        .ok_or_else(|| "--seed wants <node-id>@<host:port>".to_owned())?,
                );
            }
            "--restore" => {
                let path = arguments
                    .next()
                    .ok_or_else(|| "--restore wants a path".to_owned())?;
                source = Source::Restore(PathBuf::from(path));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other:?}\n\n{USAGE}"));
            }
            path if store.is_none() => store = Some(PathBuf::from(path)),
            extra => {
                return Err(format!(
                    "only one store may be opened, and {extra:?} is a second"
                ));
            }
        }
    }

    // A path and an address are two stores, refused for the same reason two
    // paths are: a session that quietly picked one of them is how somebody
    // writes to the wrong one.
    if at.is_some() && store.is_some() {
        return Err("a store and an address are two stores; give one".to_owned());
    }
    if at.is_some() {
        // These reach past the session into the store itself, and the protocol
        // carries scripts. Ignoring the address would run them against a store
        // in this process, which is the opposite of what was asked for.
        let reached_past_the_session = match source {
            Source::Backup(_) => Some("--backup"),
            Source::Snapshot(_) => Some("--snapshot"),
            Source::Dump(_) => Some("--dump"),
            Source::Restore(_) => Some("--restore"),
            Source::Health => Some("--health"),
            Source::Serve => Some("--serve"),
            Source::Verify(_) => Some("--verify"),
            // `--version` and `--help` name this binary, not the node at the
            // address, so an address alongside them is neither refused nor
            // consulted.
            Source::Version
            | Source::Help
            | Source::Standard
            | Source::Inline(_)
            | Source::File(_) => None,
        };
        if let Some(named) = reached_past_the_session {
            return Err(format!(
                "{named} works on a store this process opened, and --at names one it did not"
            ));
        }
    }
    // A parameter binds to a script, and these three run no script. Refused
    // rather than ignored, for the same reason an unknown flag is: a value
    // silently dropped is a value somebody believes was used.
    let runs_no_script = match source {
        Source::Backup(_) => Some("--backup"),
        Source::Snapshot(_) => Some("--snapshot"),
        Source::Dump(_) => Some("--dump"),
        Source::Restore(_) => Some("--restore"),
        Source::Health => Some("--health"),
        Source::Serve => Some("--serve"),
        Source::Verify(_) => Some("--verify"),
        Source::Version => Some("--version"),
        Source::Help => Some("--help"),
        Source::Standard | Source::Inline(_) | Source::File(_) => None,
    };
    if let Some(named) = runs_no_script
        && !parameters.is_empty()
    {
        return Err(format!(
            "--param binds a value in a script, and {named} runs none"
        ));
    }
    // A sequence means one thing next to `--backup` and another next to
    // `--restore`, and nothing at all anywhere else. Refused rather than
    // ignored: a caller who wrote `--from` expecting it to bound a read has
    // asked for something, and silence would answer with everything.
    if sequence.is_some() && !matches!(source, Source::Backup(_) | Source::Restore(_)) {
        return Err("--from bounds a --backup and --upto bounds a --restore".to_owned());
    }
    // An address given alongside something that is not serving would be read,
    // accepted, and never listened on. Refused for the reason the rest of this
    // module refuses: a value silently dropped is a value somebody believes was
    // used, and here what they believe is that a port is open.
    if serving.asked() && !matches!(source, Source::Serve) {
        return Err("an address to serve on and something else to do are two programs".to_owned());
    }
    // The same reason again: a folder named and never written to is one somebody
    // believes holds their backups.
    if serving.backups.is_some() && !matches!(source, Source::Serve) {
        return Err(
            "--backup-dir is where a serving node writes `BACKUP … TO`, and this serves nothing"
                .to_owned(),
        );
    }
    // And again: a period named on a process that holds no unseal anybody can
    // reach is a period somebody believes is protecting them.
    if serving.unseal_for.is_some() && !matches!(source, Source::Serve) {
        return Err(
            "--unseal-for is how long a serving node's unseal lasts, and this serves nothing"
                .to_owned(),
        );
    }
    // And again: a certificate named on a process that serves nothing is one
    // somebody believes is encrypting their clients.
    if serving.tls != crate::tls::Given::default() && !matches!(source, Source::Serve) {
        return Err("--tls-cert, --tls-key and --require-client-tls say how a \
             serving node speaks to its clients, and this serves nothing"
            .to_owned());
    }
    // The client's half: trusting a node by a certificate is something only a
    // connection to one can do.
    if trusted.is_some() && at.is_none() {
        return Err(
            "--tls-authority is what --at trusts a node by, and there is no --at".to_owned(),
        );
    }
    // A key is for the files this process writes: a store on disk, or a backup
    // to verify. Named for a store elsewhere or one in memory, it would encrypt
    // nothing while somebody believed it did.
    if encryption_key.is_some()
        && (at.is_some() || store.is_none())
        && !matches!(source, Source::Verify(_))
    {
        return Err(
            "--encryption-key-file encrypts a store on disk this process opens, and none was named"
                .to_owned(),
        );
    }
    // The key a backup opens under is only ever asked for by reading one.
    if backup_key.is_some() && !matches!(source, Source::Restore(_) | Source::Verify(_)) {
        return Err(
            "--backup-key-file opens a backup being restored or verified, and this reads none"
                .to_owned(),
        );
    }
    // Same reason as the line above, and what the operator believes here is
    // stronger: not that a port is open, but that this node joined a cluster.
    let told_about_a_cluster = credential.is_some()
        || key.is_some()
        || authority.is_some()
        || door.is_some()
        || !seeds.is_empty()
        || serving.join_token.is_some();
    if told_about_a_cluster && !matches!(source, Source::Serve) {
        return Err("a cluster is something a node serves in, and this serves nothing".to_owned());
    }
    // Collected here, decided there. `Told::from_parts` owns the rule that
    // separates *told nothing* from *told half*, and a second copy of it in
    // this module would be a second rule the moment either is edited.
    let cluster = tessari_wire::Told::from_parts(credential, key, authority, door, seeds)
        .map_err(|refused| refused.to_string())?;
    Ok(Asked {
        store,
        at,
        user,
        source,
        parameters,
        at_sequence: sequence,
        serving,
        authority: trusted,
        encryption_key,
        backup_key,
        cluster,
    })
}

/// One `<name>=<value>`, with the value read as a TessariQL literal.
///
/// TessariQL rather than JSON because the console already reads and writes it: what
/// an answer prints pastes back into the next statement, and a parameter written
/// the way an answer is printed closes that loop — `dec 12.34`, `2s` and
/// `datetime '…'` all say themselves.
///
/// The value is parsed **in isolation** by `tessaridb::value_of`, so it is a value
/// or it is nothing: `--param x="1; DROP TABLE users"` is refused as a literal
/// rather than smuggled in as a statement. That reader is shared with the HTTP
/// body's `parameters`, so the two surfaces cannot come to read a supplied value
/// differently.
fn parameter(given: &str) -> Result<(String, Value), String> {
    let Some((name, written)) = given.split_once('=') else {
        return Err(format!("--param wants <name>=<value>, not {given:?}"));
    };
    if name.is_empty() {
        return Err("--param wants a name before the `=`".to_owned());
    }
    let name = name.strip_prefix('$').unwrap_or(name);
    let value =
        tessaridb::value_of(written).map_err(|reason| format!("--param {name}: {reason}"))?;
    Ok((name.to_owned(), value))
}

/// Who to say we are, when a name was given.
///
/// The password is read from the environment because an argument would be
/// readable by anybody with the process table and would outlive the session in
/// the shell history.
pub fn credentials(user: Option<String>) -> Result<Option<(String, String)>, String> {
    let Some(name) = user else {
        return Ok(None);
    };
    let password = env::var(PASSWORD)
        .map_err(|_| format!("--user needs the password in {PASSWORD}, and it is not set"))?;
    Ok(Some((name, password)))
}

/// Read an unseal period written as a TessariQL duration (ADR-0092 D4).
///
/// The flag and `TESSARIDB_UNSEAL_FOR` both come through here, so the two
/// spellings cannot accept different things. Zero is refused rather than read
/// as "never": a period is how long the store stays open, and one that means
/// the opposite of what it says at its smallest value is a trap.
///
/// # Errors
///
/// A text that is not a duration, or a duration that is not positive.
pub fn unseal_period(written: &str) -> Result<core::time::Duration, String> {
    let Ok(tessari_types::Value::Duration(period)) = tessaridb::value_of(written) else {
        return Err(format!(
            "wants a duration such as 10m or 1h, not `{written}`"
        ));
    };
    let seconds = u64::try_from(period.seconds()).ok();
    match seconds.map(|seconds| core::time::Duration::new(seconds, period.nanos())) {
        Some(held) if !held.is_zero() => Ok(held),
        _ => Err(format!("wants a period longer than zero, not `{written}`")),
    }
}

/// `TESSARIDB_RETAIN_RECORDS`: how many log records a serving node keeps where
/// no `DEFINE NODE RETAIN` said (ADR-0094 D2) — a positive count, or `none` for
/// an unbounded log.
///
/// Anything else stops the start rather than falling back to the default: an
/// operator who set the variable believes the log is bounded where they said.
///
/// # Errors
///
/// Returns the sentence naming what was wrong with `written`.
pub fn retained_records(written: &str) -> Result<tessari_storage::Retention, String> {
    if written.eq_ignore_ascii_case("none") {
        return Ok(tessari_storage::Retention::Unbounded);
    }
    match written.parse::<u64>() {
        Ok(count) if count > 0 => Ok(tessari_storage::Retention::Keep(
            tessari_types::Sequence::new(count),
        )),
        _ => Err(format!(
            "wants a number of records above zero, or `none`, not `{written}`"
        )),
    }
}

#[cfg(test)]
mod tests;
