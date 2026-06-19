//! Guards: the default Linear provider is offline, and the manifest declares no
//! HTTP/network client dependency. No TOML parser and no new dependency — a plain
//! text scan of `Cargo.toml` (included at compile time).

use chrono::Utc;
use lane::board::linear::{LinearProvider, NoLinearProvider};
use lane::model::Provenance;

#[test]
fn default_linear_provider_is_offline() {
    let provider = NoLinearProvider;
    assert!(provider.issue_for("LQOS-1").is_none());
    let freshness = provider.freshness(Utc::now());
    assert_eq!(freshness.provenance, Provenance::Unknown);
}

#[test]
fn manifest_declares_no_http_network_deps() {
    // Compile-time include of this crate's manifest; no runtime file IO, no TOML parser.
    let manifest = include_str!("../Cargo.toml");
    // The locking core is permanently offline and std-only: no network/HTTP client, no
    // async runtime, no embedded DB, no raw-syscall shim. Adapters that add Git/Linear/
    // 1Password/tmux/overseer are future, separately-gated slices that live OUTSIDE the
    // core — they are not declared here.
    const FORBIDDEN: &[&str] = &[
        // HTTP / network
        "reqwest",
        "hyper",
        "ureq",
        "graphql_client",
        "isahc",
        "surf",
        // async runtimes
        "tokio",
        "async-std",
        "async_std",
        "smol",
        // embedded DB / KV
        "rusqlite",
        "sled",
        "diesel",
        "sqlx",
        "rocksdb",
        // raw-syscall shims (Slice 2 is std-only — no O_NOFOLLOW/libc path)
        "libc",
        "rustix",
        "nix",
    ];
    for line in manifest.lines() {
        // A dependency line is `name = "..."` or `name = { ... }`; the key is left of `=`.
        let key = line.split('=').next().map(str::trim).unwrap_or_default();
        for dep in FORBIDDEN {
            assert!(
                key != *dep,
                "Cargo.toml must not declare the network client dependency `{dep}`"
            );
        }
    }
}
