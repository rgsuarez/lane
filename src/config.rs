//! `$LANE_ROOT/config.toml` — lane's app config reader (first introduced in Slice 4).
//!
//! Config is OPERATOR-owned, ADAPTER-only state: it names secret **role keys** (mapped
//! to opaque `op://` references or `env:` pointers — never values, never human labels)
//! and Linear adapter settings. Local verbs never load it; a missing file is simply the
//! defaults. Because the `[linear] api_url` value controls where a resolved secret is
//! SENT, the file is read through the same object-guarded reader as claim state
//! ([`crate::lock::record::read_guarded`]): a symlinked / foreign-owned / non-regular
//! `config.toml` fails closed (exit 2) rather than silently redirecting credentials.
//!
//! The locking core never imports this module (enforced by the source scan in
//! `tests/no_network_guard.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::LaneError;
use crate::lock::record;
use crate::lock::FsOps;

/// File name under `$LANE_ROOT`.
pub const CONFIG_FILE: &str = "config.toml";
/// Default Linear GraphQL endpoint (overridable via `[linear] api_url`, e.g. by
/// hermetic tests pointing at a loopback fixture).
pub const DEFAULT_LINEAR_API_URL: &str = "https://api.linear.app/graphql";
/// Default TTL for the Linear read cache.
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 300;
/// The secret role the Linear adapter resolves.
pub const ROLE_LINEAR_API: &str = "linear_api";

/// Parsed `config.toml`. Unknown keys are ignored everywhere (additive evolution).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LaneConfig {
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub linear: LinearConfig,
}

/// `[secrets]` — role keys and provider settings. References are opaque pointers
/// (`op://<vault>/<item>/<field>` or `env:VARNAME`); values are NEVER stored here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecretsConfig {
    /// Optional 1Password account passed as `op --account <value>` (this machine has
    /// multiple accounts; pinning prevents resolving from the wrong one).
    #[serde(default)]
    pub op_account: Option<String>,
    /// role key → opaque reference.
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
}

/// `[linear]` — adapter settings.
#[derive(Debug, Clone, Deserialize)]
pub struct LinearConfig {
    #[serde(default = "default_api_url")]
    pub api_url: String,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            api_url: default_api_url(),
            cache_ttl_seconds: default_cache_ttl(),
        }
    }
}

fn default_api_url() -> String {
    DEFAULT_LINEAR_API_URL.to_string()
}

fn default_cache_ttl() -> u64 {
    DEFAULT_CACHE_TTL_SECONDS
}

impl LaneConfig {
    /// The config file path under a lane root.
    pub fn path_under(root: &Path) -> PathBuf {
        root.join(CONFIG_FILE)
    }

    /// Load config from `<root>/config.toml` via the object-guarded reader.
    /// Missing file ⇒ `Ok(defaults)`. Malformed TOML ⇒ `Malformed` (exit 2).
    /// Symlink / non-regular / foreign owner ⇒ fail closed (exit 2, from the guard).
    pub fn load(root: &Path, expected_uid: u32, fs: &dyn FsOps) -> Result<Self, LaneError> {
        let path = Self::path_under(root);
        let Some(text) = record::read_guarded(&path, root, expected_uid, fs)? else {
            return Ok(Self::default());
        };
        toml::from_str(&text).map_err(|e| LaneError::Malformed {
            path,
            detail: format!("unparseable config: {e}"),
        })
    }

    /// The opaque reference mapped to a role key, if configured.
    pub fn role_reference(&self, role: &str) -> Option<&str> {
        self.secrets.roles.get(role).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::StdFs;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn uid_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().uid()
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LaneConfig::load(dir.path(), uid_of(dir.path()), &StdFs).unwrap();
        assert!(cfg.secrets.roles.is_empty());
        assert!(cfg.secrets.op_account.is_none());
        assert_eq!(cfg.linear.api_url, DEFAULT_LINEAR_API_URL);
        assert_eq!(cfg.linear.cache_ttl_seconds, DEFAULT_CACHE_TTL_SECONDS);
        assert!(cfg.role_reference(ROLE_LINEAR_API).is_none());
    }

    #[test]
    fn full_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
[secrets]
op_account = "my.1password.com"
[secrets.roles]
linear_api = "op://Vault/Item/credential"
[linear]
api_url = "http://127.0.0.1:9/graphql"
cache_ttl_seconds = 7
"#,
        )
        .unwrap();
        let cfg = LaneConfig::load(dir.path(), uid_of(dir.path()), &StdFs).unwrap();
        assert_eq!(cfg.secrets.op_account.as_deref(), Some("my.1password.com"));
        assert_eq!(
            cfg.role_reference("linear_api"),
            Some("op://Vault/Item/credential")
        );
        assert_eq!(cfg.linear.api_url, "http://127.0.0.1:9/graphql");
        assert_eq!(cfg.linear.cache_ttl_seconds, 7);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(CONFIG_FILE),
            "[future_section]\nx = 1\n[linear]\nnew_knob = true\n",
        )
        .unwrap();
        let cfg = LaneConfig::load(dir.path(), uid_of(dir.path()), &StdFs).unwrap();
        assert_eq!(cfg.linear.api_url, DEFAULT_LINEAR_API_URL);
    }

    #[test]
    fn malformed_toml_is_malformed_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), "not = [valid\n").unwrap();
        let err = LaneConfig::load(dir.path(), uid_of(dir.path()), &StdFs).unwrap_err();
        assert!(matches!(err, LaneError::Malformed { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn symlinked_config_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("elsewhere.toml");
        fs::write(&real, "[linear]\napi_url = \"http://evil/\"\n").unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join(CONFIG_FILE)).unwrap();
        let err = LaneConfig::load(dir.path(), uid_of(dir.path()), &StdFs).unwrap_err();
        assert!(matches!(err, LaneError::Identity(_)));
        assert_eq!(err.exit_code(), 2);
    }
}
