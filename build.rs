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

    // antex has no release cadence of its own — it follows whatever Ante and
    // opencode are on — so its version is the day it was built. Compiled in,
    // not read at runtime: the binary should not claim a different version
    // tomorrow than it is now.
    println!("cargo:rustc-env=ANTEX_VERSION={}", build_date());
}

/// Local `YYYY-MM-DD`. Falls back to `unknown` rather than failing the build.
fn build_date() -> String {
    let out = std::process::Command::new("date").arg("+%Y-%m-%d").output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            println!("cargo:warning=`date` unavailable; antex version will read as unknown");
            "unknown".to_string()
        }
    }
}

/// The `version` line of the named `[[package]]` entry.
fn find_version(lock: &str, package: &str) -> Option<String> {
    let marker = format!("name = \"{package}\"");
    let after = &lock[lock.find(&marker)? + marker.len()..];
    let value = &after[after.find("version = \"")? + "version = \"".len()..];
    Some(value[..value.find('"')?].to_string())
}
