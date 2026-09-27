//! Refuse to build the store for macOS unless its syncs reach the medium.
//!
//! On macOS, `fsync` and `fdatasync` hand the data to the drive and return while
//! it may still sit in the drive's write cache; only `fcntl(F_FULLFSYNC)` asks the
//! drive to flush. The engine issues that call only when compiled with
//! `HAVE_FULLFSYNC`, and the engine crate's own build never defines it. A macOS
//! store built without it acknowledges `Durability::PowerLossSafe` commits that a
//! power loss can take back, and nothing at run time can tell (Q-823).
//!
//! The engine's C++ is compiled by the `cc` crate, which reads its flags from the
//! first variable present in the order checked below. The workspace sets the
//! target-specific one in `.cargo/config.toml`; this refuses any other build —
//! another workspace, a changed environment — that would silently drop it.

use std::env;

const FULL_FLUSH: &str = "-DHAVE_FULLFSYNC";

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();
    let underscored = target.replace('-', "_");
    let kind = if target == host { "HOST" } else { "TARGET" };
    // The order the C++ build resolves its flags in: the first one present wins
    // and the rest are not read, so a flag in a later one would not reach it.
    let candidates = [
        format!("CXXFLAGS_{target}"),
        format!("CXXFLAGS_{underscored}"),
        format!("{kind}_CXXFLAGS"),
        "CXXFLAGS".to_owned(),
    ];
    for name in &candidates {
        println!("cargo::rerun-if-env-changed={name}");
    }
    if !target.contains("apple-darwin") {
        return;
    }
    let used = candidates.iter().find_map(|name| env::var(name).ok());
    let flushes = used
        .as_deref()
        .is_some_and(|flags| flags.split_whitespace().any(|flag| flag == FULL_FLUSH));
    if !flushes {
        println!(
            "cargo::error=building the store for {target} without {FULL_FLUSH}: the engine would sync \
             with fdatasync, which on macOS does not flush the drive's cache, so a PowerLossSafe \
             commit could be lost on power loss. Set CXXFLAGS_{underscored}=\"{FULL_FLUSH}\" (the \
             workspace's .cargo/config.toml does)."
        );
    }
}
