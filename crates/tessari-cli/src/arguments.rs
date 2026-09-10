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
  --param <name>=<value> bind $name to <value>, written as TessariQL; repeatable
  -e, --execute <script> run this and exit
  -f, --file <file> run this file and exit
  --backup <file> write the store's log to <file> and exit
  --verify <file> read <file> and say what it holds, changing nothing
  --from <n>      with --backup: write only what happened at or after <n>
  --upto <n>      with --restore: stop replaying after sequence <n>
  --restore <file> replay <file> into an empty store and exit
  --health        say whether the store is well, and exit non-zero if not
  -V, --version   say which build this is, and exit
  -h, --help      this

with neither -e nor -f, statements are read from standard input: a prompt when
that is a terminal, a script when it is a pipe.

serving a store that has no users yet, TESSARIDB_INITIAL_USER and
TESSARIDB_INITIAL_PASSWORD declare that user as a store-wide owner and close the
store. Both or neither: half of them is refused rather than started, because a
node that came up open because a variable was misspelled looks exactly like one
that came up correctly. A store that already has users ignores them, so a
container may carry them on every restart, and they are not a way to reset a
password.";

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
    Ok(Asked {
        store,
        at,
        user,
        source,
        parameters,
        at_sequence: sequence,
        serving,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessaridb::Number;

    use super::{Asked, PASSWORD, Source, USAGE, Value, credentials, parse};

    fn asked(arguments: &[&str]) -> Result<Asked, String> {
        parse(arguments.iter().map(|held| (*held).to_owned()))
    }

    #[test]
    fn no_arguments_is_an_in_memory_store_read_from_standard_input() {
        let held = asked(&[]).expect("defaults");
        assert!(held.store.is_none());
        assert!(matches!(held.source, Source::Standard));
    }

    #[test]
    fn asking_for_the_version_is_its_own_thing_to_do() {
        assert!(matches!(
            asked(&["--version"]).expect("--version").source,
            Source::Version
        ));
        assert!(matches!(
            asked(&["-V"]).expect("-V").source,
            Source::Version
        ));
    }

    #[test]
    fn asking_for_the_help_is_a_request_and_not_a_refusal() {
        // It used to be `Err(USAGE)`, which `main` printed to standard error
        // with a non-zero status — so `tessaridb --help | grep serve` came back
        // empty and any packaging check that runs `--help` failed. A parse
        // *error* is still an error; being asked for the usage is not one.
        assert!(matches!(
            asked(&["--help"]).expect("--help").source,
            Source::Help
        ));
        assert!(matches!(asked(&["-h"]).expect("-h").source, Source::Help));
    }

    #[test]
    fn a_misspelled_flag_is_still_a_refusal_and_still_carries_the_usage() {
        // The other half of the split above: the usage text did double duty as
        // both the answer to `--help` and the body of a parse error, and only
        // the first of those changed channel.
        let complaint = asked(&["--nonsense"]).expect_err("a typo");
        assert!(
            complaint.contains("--nonsense") && complaint.contains("usage:"),
            "a refusal that names neither the flag nor the usage: {complaint}"
        );
    }

    #[test]
    fn a_misspelled_flag_beside_the_version_is_still_reported() {
        // The reason `--version` is read by the parser rather than
        // short-circuited ahead of it. A binary that answered the version and
        // swallowed a typo would be the one place this module lets an
        // unrecognised option through.
        let complaint = asked(&["--version", "--serv", "127.0.0.1:0"]).expect_err("a typo");
        assert!(
            complaint.contains("--serv"),
            "the typo is not named: {complaint}"
        );
    }

    #[test]
    fn a_parameter_bound_for_a_version_that_runs_no_script_is_refused() {
        let complaint = asked(&["--version", "--param", "x=1"]).expect_err("no script");
        assert!(
            complaint.contains("--version"),
            "the refusal does not say which flag runs no script: {complaint}"
        );
    }

    #[test]
    fn a_bare_argument_is_the_store() {
        let held = asked(&["./data"]).expect("a path");
        assert_eq!(held.store.as_deref(), Some(std::path::Path::new("./data")));
    }

    #[test]
    fn a_script_and_a_file_are_told_apart() {
        assert!(matches!(
            asked(&["-e", "SELECT * FROM users;"])
                .expect("a script")
                .source,
            Source::Inline(_)
        ));
        assert!(matches!(
            asked(&["-f", "setup.tessariql"]).expect("a file").source,
            Source::File(_)
        ));
    }

    #[test]
    fn a_misspelled_option_is_refused_rather_than_read_as_a_path() {
        // Otherwise `--excute 'DELETE …'` opens a store called `--excute`.
        assert!(asked(&["--excute", "x"]).is_err());
        assert!(asked(&["-e"]).is_err());
        assert!(asked(&["-f"]).is_err());
    }

    #[test]
    fn a_second_store_is_refused_rather_than_silently_ignored() {
        assert!(asked(&["./one", "./two"]).is_err());
    }

    #[test]
    fn a_path_and_an_address_are_two_stores_and_are_refused_as_one() {
        // Quietly preferring one of them is how somebody writes to the wrong
        // store, which is the same reason two paths are refused.
        assert!(asked(&["./data", "--at", "127.0.0.1:7654"]).is_err());
        assert!(asked(&["--at", "127.0.0.1:7654", "./data"]).is_err());
        assert!(asked(&["--at", "127.0.0.1:7654"]).is_ok());
        assert!(asked(&["--at"]).is_err());
    }

    #[test]
    fn what_reaches_past_the_session_is_refused_over_an_address() {
        // The protocol carries scripts. Ignoring the address and running these
        // against a store in this process is the opposite of what was asked.
        for reaching in [
            vec!["--at", "127.0.0.1:7654", "--health"],
            vec!["--at", "127.0.0.1:7654", "--backup", "out"],
            vec!["--at", "127.0.0.1:7654", "--restore", "in"],
            vec!["--at", "127.0.0.1:7654", "--serve", "0.0.0.0:1"],
        ] {
            let complaint = asked(&reaching).expect_err("refused");
            assert!(
                complaint.contains("--at names one it did not"),
                "{complaint}"
            );
        }
        // A script is not, because a script is what the protocol carries.
        assert!(asked(&["--at", "127.0.0.1:7654", "-e", "SELECT 1;"]).is_ok());
    }

    #[test]
    fn a_password_is_never_read_from_an_argument() {
        // It would be in the process table for anybody on the machine and in the
        // shell history afterwards. `--password` is not a flag, so it is refused
        // the way any unknown option is.
        assert!(asked(&["--password", "hunter2"]).is_err());
        let held = asked(&["--user", "ada"]).expect("a name");
        assert_eq!(held.user.as_deref(), Some("ada"));
    }

    #[test]
    fn a_name_without_the_password_in_the_environment_is_refused() {
        // Signing in anonymously after a failed sign-in would answer a different
        // question than the one that was asked.
        let complaint = credentials(Some("ada".to_owned()));
        match std::env::var(PASSWORD) {
            Ok(_) => assert!(complaint.is_ok()),
            Err(_) => assert!(
                complaint.expect_err("refused").contains(PASSWORD),
                "the refusal should say where the password comes from"
            ),
        }
    }

    #[test]
    fn serving_takes_an_address_of_its_own() {
        let held = asked(&["./data", "--serve", "0.0.0.0:7654"]).expect("an address");
        assert!(matches!(held.source, Source::Serve));
        assert_eq!(held.serving.wire.as_deref(), Some("0.0.0.0:7654"));
        assert_eq!(held.serving.http, None);
        assert!(asked(&["--serve"]).is_err());
    }

    #[test]
    fn each_surface_takes_its_own_address_and_they_may_be_asked_for_together() {
        let held = asked(&[
            "./data",
            "--serve",
            "0.0.0.0:7654",
            "--http",
            "127.0.0.1:8000",
        ])
        .expect("two addresses");
        assert!(matches!(held.source, Source::Serve));
        assert_eq!(held.serving.wire.as_deref(), Some("0.0.0.0:7654"));
        assert_eq!(held.serving.http.as_deref(), Some("127.0.0.1:8000"));

        // Either alone, because a process holds whichever subset was named.
        let only_http = asked(&["./data", "--http", "127.0.0.1:8000"]).expect("one address");
        assert_eq!(only_http.serving.wire, None);
        assert_eq!(only_http.serving.http.as_deref(), Some("127.0.0.1:8000"));

        assert!(asked(&["--http"]).is_err());
    }

    #[test]
    fn an_address_next_to_something_that_is_not_serving_is_refused() {
        // Not ignored. A port that was named and never opened is worse than one
        // that was refused, because nothing says which happened.
        let refusal = asked(&["./data", "--http", "127.0.0.1:8000", "-e", "SELECT 1;"])
            .expect_err("two programs");
        assert!(
            refusal.contains("two programs"),
            "the refusal should say why: {refusal}"
        );
    }

    #[test]
    fn a_parameter_is_read_as_a_tessariql_value() {
        let held = asked(&[
            "-e",
            "SELECT * FROM users WHERE name = $who;",
            "--param",
            "who='ada'",
        ])
        .expect("a parameter");
        assert_eq!(
            held.parameters.get("who"),
            Some(&Value::String("ada".to_owned()))
        );
    }

    #[test]
    fn a_parameter_keeps_the_kinds_json_would_have_flattened() {
        let held = asked(&[
            "-e",
            "SELECT 1;",
            "--param",
            "cost=dec 12.34",
            "--param",
            "span=1h30m",
        ])
        .expect("two parameters");
        assert!(matches!(
            held.parameters.get("cost"),
            Some(Value::Number(Number::Decimal(_)))
        ));
        assert!(matches!(
            held.parameters.get("span"),
            Some(Value::Duration(_))
        ));
    }

    #[test]
    fn a_parameter_may_be_written_with_its_marker() {
        // `--param $who='ada'` is what somebody types after reading the script,
        // and refusing it would be pedantry with a shell-quoting trap attached.
        let held = asked(&["-e", "SELECT 1;", "--param", "$who='ada'"]).expect("a parameter");
        assert!(held.parameters.contains_key("who"));
    }

    #[test]
    fn a_parameter_that_is_not_a_value_is_refused() {
        // The rule one layer out from the grammar: a value is a value or it is
        // nothing, so a statement smuggled in as one is refused here rather
        // than pasted into a script.
        for given in [
            "who=1; DROP TABLE users",
            "who=SELECT * FROM users",
            "who=name",
        ] {
            assert!(
                asked(&["-e", "SELECT 1;", "--param", given]).is_err(),
                "{given} was accepted as a value"
            );
        }
    }

    #[test]
    fn a_parameter_needs_a_name_and_a_value() {
        assert!(asked(&["--param"]).is_err());
        assert!(asked(&["--param", "who"]).is_err());
        assert!(asked(&["--param", "='ada'"]).is_err());
    }

    #[test]
    fn a_sequence_bounds_a_backup_or_a_restore_and_nothing_else() {
        assert_eq!(
            asked(&["./data", "--backup", "./out", "--from", "42"])
                .expect("a bounded backup")
                .at_sequence,
            Some(42)
        );
        assert_eq!(
            asked(&["./data", "--restore", "./in", "--upto", "7"])
                .expect("a bounded restore")
                .at_sequence,
            Some(7)
        );
        // Elsewhere it means nothing, and silence would answer with everything.
        assert!(asked(&["./data", "--from", "42"]).is_err());
        assert!(asked(&["./data", "--health", "--upto", "7"]).is_err());
        assert!(asked(&["./data", "--backup", "./out", "--from", "soon"]).is_err());
    }

    #[test]
    fn verifying_needs_a_path_and_no_store() {
        assert!(matches!(
            asked(&["--verify", "./held"]).expect("a path").source,
            Source::Verify(_)
        ));
        assert!(asked(&["--verify"]).is_err());
        // It reads a file, so an address is a store it was not asked about.
        assert!(asked(&["--at", "127.0.0.1:1", "--verify", "./held"]).is_err());
    }

    #[test]
    fn a_parameter_is_refused_where_no_script_runs() {
        // Ignoring it would leave somebody believing a value was used.
        for source in [
            vec!["./data", "--health"],
            vec!["./data", "--backup", "./out"],
            vec!["./data", "--serve", "127.0.0.1:0"],
        ] {
            let mut arguments = source.clone();
            arguments.extend(["--param", "who='ada'"]);
            assert!(
                asked(&arguments).is_err(),
                "{source:?} accepted a parameter"
            );
        }
    }

    /// The usage names every option form the parser accepts.
    ///
    /// # Why this reads the source and not a list
    ///
    /// `--help` listed four forms fewer than the binary accepted — `--execute`,
    /// `--file`, `-V` and `-h` all worked and none was printed — and the
    /// documentation site published the complete table under the sentence
    /// *"this is the binary's own usage"*, which was therefore false in one
    /// direction, with the incomplete side being the binary (Q-367).
    ///
    /// A list of forms written beside the parser would pin two lists to each
    /// other and neither to the binary, so this reads the `match` itself. The
    /// scan stops at the test module, so a form spelled inside a test is not
    /// mistaken for one the parser takes.
    #[test]
    fn the_usage_names_every_option_the_parser_accepts() {
        let source = include_str!("arguments.rs");
        let parser = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        let mut forms = Vec::new();
        for line in parser.lines().filter(|line| line.contains("=>")) {
            let mut rest = line;
            while let Some(open) = rest.find('"') {
                rest = &rest[open + 1..];
                let Some(close) = rest.find('"') else { break };
                let (form, after) = rest.split_at(close);
                rest = &after[1..];
                if form.starts_with('-') && form.len() > 1 {
                    forms.push(form.to_owned());
                }
            }
        }
        assert!(forms.len() > 10, "the scan found almost nothing: {forms:?}");
        let missing: Vec<&String> = forms
            .iter()
            .filter(|form| !USAGE.contains(form.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "the parser accepts option forms the usage does not print: {missing:?}"
        );
    }
}
