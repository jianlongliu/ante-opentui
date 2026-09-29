//! Bake the compiled-in `ante-sdk` version into the binary.
//!
//! Ante is the shim's only real dependency, and the only coupling is the
//! protocol. When the two drift apart every request fails in a way the TUI
//! renders as "nothing happens"; knowing which SDK this binary was built
//! against is what turns that into a readable log line at startup.

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("Cargo.lock");
    let text = fs::read_to_string(&lock).unwrap_or_default();
    let version = find_version(&text, "ante-sdk").unwrap_or_else(|| {
        println!("cargo:warning=ante-sdk missing from Cargo.lock; startup self-check will be vague");
        "unknown".to_string()
    });
    println!("cargo:rustc-env=ANTE_SDK_VERSION={version}");
}

/// The `version` line of the named `[[package]]` entry.
fn find_version(lock: &str, package: &str) -> Option<String> {
    let marker = format!("name = \"{package}\"");
    let after = &lock[lock.find(&marker)? + marker.len()..];
    let value = &after[after.find("version = \"")? + "version = \"".len()..];
    Some(value[..value.find('"')?].to_string())
}
