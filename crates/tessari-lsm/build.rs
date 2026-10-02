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
//!
//! It also compiles `src/cipher.cc`, the engine's encryption provider for an
//! encrypted store (ADR-0108 D7), against the engine's own headers. The engine's
//! build crate exports where its sources are (`DEP_ROCKSDB_CARGO_MANIFEST_DIR`)
//! to the crates that depend on it directly, which is why this crate names it.

use std::env;

const FULL_FLUSH: &str = "-DHAVE_FULLFSYNC";

fn main() {
    cipher();
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

/// Compile the encryption provider against the headers of the engine linked.
fn cipher() {
    println!("cargo::rerun-if-changed=src/cipher.cc");
    let Ok(engine) = env::var("DEP_ROCKSDB_CARGO_MANIFEST_DIR") else {
        println!(
            "cargo::error=the engine's build did not say where its sources are \
             (DEP_ROCKSDB_CARGO_MANIFEST_DIR); the encryption provider cannot be compiled"
        );
        return;
    };
    let include = std::path::Path::new(&engine)
        .join("rocksdb")
        .join("include");
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .include(include)
        .file("src/cipher.cc")
        .compile("tessari_lsm_cipher");
}
