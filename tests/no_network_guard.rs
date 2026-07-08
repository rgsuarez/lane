//! Guards: the default Linear provider is offline; the LOCKING CORE is std-only and
//! network-free. Since Slice 4 the crate's `[dependencies]` may contain exactly the
//! `ADAPTER_ONLY` crates (each with a written justification), which only adapter
//! modules may import — the manifest scan and the core source scan below enforce
//! both halves of that law. The scans are plain text (compile-time `include_str!`
//! for the manifest; `std::fs` walks for sources) — no TOML parser in this test.

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

/// Adapter-only dependencies: allowed in `[dependencies]` for modules OUTSIDE the
/// locking core. Each entry carries its justification; the source scan below pins
/// where they may be imported. Growing this list is a DELIBERATE act (Slice-gated,
/// justified in Cargo.toml and here).
const ADAPTER_ONLY: &[(&str, &str)] = &[(
    "toml",
    "Slice 4 (ZER-85): $LANE_ROOT/config.toml parsing for src/config.rs; parse-only, serde-native, network-free",
)];

/// Never allowed anywhere in the manifest: HTTP clients (other than an ADAPTER_ONLY
/// grant), async runtimes, embedded DBs, raw-syscall shims.
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
    // raw-syscall shims (the core is std-only — no O_NOFOLLOW/libc path)
    "libc",
    "rustix",
    "nix",
];

#[test]
fn manifest_declares_no_http_network_deps() {
    // Compile-time include of this crate's manifest; no runtime file IO.
    let manifest = include_str!("../Cargo.toml");
    // Sanity: the allowlist and the forbidden list are disjoint, and every allowlisted
    // crate actually appears in the manifest (keeps the allowlist honest and pruned).
    for (dep, _why) in ADAPTER_ONLY {
        assert!(
            !FORBIDDEN.contains(dep),
            "`{dep}` cannot be both ADAPTER_ONLY and FORBIDDEN"
        );
        let declared = manifest
            .lines()
            .any(|line| line.split('=').next().map(str::trim).unwrap_or_default() == *dep);
        assert!(
            declared,
            "ADAPTER_ONLY lists `{dep}` but Cargo.toml does not declare it — prune the allowlist"
        );
    }
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

/// The other half of the law: the locking core (`src/lock/**`) and the commit guard
/// (`src/hook.rs`) never import an adapter module or an adapter-only/network crate.
/// A plain line scan (comment lines skipped) in the same spirit as the manifest scan.
#[test]
fn core_sources_import_no_adapter_or_network_code() {
    const BANNED_TOKENS: &[&str] = &[
        "ureq",
        "crate::linear",
        "crate::secrets",
        "crate::config",
        "toml::",
        "use toml",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = rs_files_under(&root.join("src/lock"));
    files.push(root.join("src/hook.rs"));
    assert!(
        files.len() > 5,
        "source scan found suspiciously few core files — walk is broken"
    );
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()));
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for token in BANNED_TOKENS {
                assert!(
                    !trimmed.contains(token),
                    "{}:{}: locking core references `{token}` — the core is permanently \
                     offline and never imports adapter modules or network-capable crates",
                    file.display(),
                    n + 1
                );
            }
        }
    }
}

/// Recursive `.rs` listing via std only.
fn rs_files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            out.extend(rs_files_under(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}
