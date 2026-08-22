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

use bgv_db::{Parameters, Value};
use bgv_db_ql::{ExprKind, parse_expression};

pub const USAGE: &str = "\
usage: bgv [<path> | --at <host:port>] [-e <script> | -f <file>]

  <path>          a store on disk; omitted, the store is in memory and is lost
  --at <host:port> a running node, instead of a store in this process
  --user <name>   sign in as this user; the password comes from BGV_PASSWORD,
                  never from an argument, which the process table would publish
  --serve <host:port> serve this store over the wire protocol until stopped
  --param <name>=<value> bind $name to <value>, written as bgvQL; repeatable
  -e <script>     run this and exit
  -f <file>       run this file and exit
  --backup <file> write the store's log to <file> and exit
  --restore <file> replay <file> into an empty store and exit
  --health        say whether the store is well, and exit non-zero if not
  --help          this

with neither -e nor -f, statements are read from standard input: a prompt when
that is a terminal, a script when it is a pipe.";

/// Where the password is read from.
///
/// Not an argument. An argument is in the process table for anybody on the
/// machine to read and in the shell history afterwards, which is a defect rather
/// than the convenience it looks like.
pub const PASSWORD: &str = "BGV_PASSWORD";

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
    /// Serve this store over the wire protocol.
    Serve(String),
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
    let mut parameters = Parameters::new();
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Err(USAGE.to_owned()),
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
                source = Source::Serve(address);
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
            Source::Serve(_) => Some("--serve"),
            Source::Standard | Source::Inline(_) | Source::File(_) => None,
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
        Source::Serve(_) => Some("--serve"),
        Source::Standard | Source::Inline(_) | Source::File(_) => None,
    };
    if let Some(named) = runs_no_script
        && !parameters.is_empty()
    {
        return Err(format!(
            "--param binds a value in a script, and {named} runs none"
        ));
    }
    Ok(Asked {
        store,
        at,
        user,
        source,
        parameters,
    })
}

/// One `<name>=<value>`, with the value read as a bgvQL literal.
///
/// bgvQL rather than JSON because the console already reads and writes it: what
/// an answer prints pastes back into the next statement, and a parameter written
/// the way an answer is printed closes that loop — `dec 12.34`, `2s` and
/// `datetime '…'` all say themselves.
///
/// The value is parsed **in isolation**, so it is a value or it is nothing:
/// `--param x="1; DROP TABLE users"` is refused as a literal rather than
/// smuggled in as a statement. That is the grammar's rule one layer out, and it
/// is why this does not simply paste the text into the script.
fn parameter(given: &str) -> Result<(String, Value), String> {
    let Some((name, written)) = given.split_once('=') else {
        return Err(format!("--param wants <name>=<value>, not {given:?}"));
    };
    if name.is_empty() {
        return Err("--param wants a name before the `=`".to_owned());
    }
    let name = name.strip_prefix('$').unwrap_or(name);
    let value = literal(written).ok_or_else(|| {
        format!("--param {name}: {written:?} is not a value bgvQL can read on its own")
    })?;
    Ok((name.to_owned(), value))
}

/// The value a piece of text denotes, when it denotes one by itself.
///
/// A read, a path and anything needing a record are not values here — an
/// argument that had to consult the store to say what it is would be a statement
/// wearing a value's clothes.
fn literal(written: &str) -> Option<Value> {
    match parse_expression(written).ok()?.kind {
        ExprKind::Literal(value) => Some(value),
        _ => None,
    }
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

    use bgv_db::Number;

    use super::{Asked, PASSWORD, Source, Value, credentials, parse};

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
            asked(&["-f", "setup.bgvql"]).expect("a file").source,
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
        assert!(matches!(
            asked(&["./data", "--serve", "0.0.0.0:7654"])
                .expect("an address")
                .source,
            Source::Serve(_)
        ));
        assert!(asked(&["--serve"]).is_err());
    }

    #[test]
    fn a_parameter_is_read_as_a_bgvql_value() {
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
}
