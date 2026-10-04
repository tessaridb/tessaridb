//! Reading the command line.
//!
//! # An unknown option is an error rather than something ignored
//!
//! A session opened with a misspelled flag that silently used the default is how
//! somebody writes to the wrong store. The same reasoning runs through the rest
//! of this module: two stores are refused rather than ordered, and asking for a
//! store operation over an address is refused rather than quietly run against a
//! store in this process.

mod source;
mod usage;
mod values;

use std::env;
use std::path::PathBuf;

pub use source::Source;
use tessaridb::{Parameters, Value};
pub use usage::USAGE;
use values::parameter;
pub use values::{credentials, retained_records, unseal_period};

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

#[cfg(test)]
mod tests;
