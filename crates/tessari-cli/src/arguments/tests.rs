#![allow(clippy::panic)]

use tessaridb::Number;

use super::{Asked, PASSWORD, Source, USAGE, Value, credentials, parse};

mod cluster;
mod parameters;
mod surface;

fn asked(arguments: &[&str]) -> Result<Asked, String> {
    parse(arguments.iter().map(|held| (*held).to_owned()))
}
