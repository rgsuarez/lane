//! TTL'd Linear read cache — DERIVED, DISPOSABLE, NON-AUTHORITATIVE state under
//! `$LANE_ROOT/cache/linear/`.
//!
//! Boundary with the guarded reader: `record::read_guarded` is the trust boundary
//! for state that gates mutations; this cache is disposable derived data whose worst
//! corruption outcome is a refetch, and it never feeds a trust decision — so it is
//! read with plain serde and any missing/corrupt/expired content silently refetches.
//! Writes are best-effort (temp + rename; a failure degrades to a stderr warning,
//! never an error) and no core verb ever touches this directory.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Cache directory under the lane root.
pub const CACHE_DIR: &str = "cache/linear";
/// The `lane pull` viewer-issues cache file.
pub const VIEWER_ISSUES_FILE: &str = "viewer-issues.json";
/// The board issues-by-key cache file.
pub const ISSUES_BY_KEY_FILE: &str = "issues-by-key.json";

/// One cached payload with its fetch time.
#[derive(Debug, Serialize, Deserialize)]
pub struct CacheEnvelope<T> {
    pub fetched_at: DateTime<Utc>,
    pub payload: T,
}

/// The cache directory path under a lane root.
pub fn cache_dir(root: &Path) -> PathBuf {
    root.join(CACHE_DIR)
}

/// Read a cache file iff it exists, parses, and is younger than `ttl_seconds`.
/// Anything else — missing, corrupt, expired, or a future timestamp — is `None`
/// (refetch), never an error.
pub fn read_fresh<T: DeserializeOwned>(
    path: &Path,
    ttl_seconds: u64,
    now: DateTime<Utc>,
) -> Option<CacheEnvelope<T>> {
    let text = fs::read_to_string(path).ok()?;
    let envelope: CacheEnvelope<T> = serde_json::from_str(&text).ok()?;
    let age = now.signed_duration_since(envelope.fetched_at);
    let ttl = Duration::seconds(ttl_seconds.min(i64::MAX as u64) as i64);
    if age < Duration::zero() || age > ttl {
        return None;
    }
    Some(envelope)
}

/// Best-effort write (dir create + temp-in-same-dir + rename, pid-suffixed temp so
/// concurrent writers never collide; last-write-wins is fine for a cache). Returns
/// a warning string on failure — callers surface it on stderr, never fail on it.
pub fn write<T: Serialize>(path: &Path, payload: &T, now: DateTime<Utc>) -> Option<String> {
    let envelope = CacheEnvelope {
        fetched_at: now,
        payload,
    };
    let attempt = (|| -> std::io::Result<()> {
        let json = serde_json::to_string(&envelope)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("cache"),
            std::process::id()
        ));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    attempt
        .err()
        .map(|e| format!("linear cache write failed ({}): {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/linear/viewer-issues.json");
        let now = Utc::now();
        assert!(write(&path, &vec!["a".to_string()], now).is_none());

        let hit: CacheEnvelope<Vec<String>> = read_fresh(&path, 300, now).expect("fresh");
        assert_eq!(hit.payload, vec!["a".to_string()]);

        // Expired: pretend we read far in the future.
        let later = now + Duration::seconds(301);
        assert!(read_fresh::<Vec<String>>(&path, 300, later).is_none());

        // Future timestamp (clock skew / tamper): refetch.
        let earlier = now - Duration::seconds(10);
        assert!(read_fresh::<Vec<String>>(&path, 300, earlier).is_none());
    }

    #[test]
    fn missing_and_corrupt_are_silent_misses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(read_fresh::<Vec<String>>(&path, 300, Utc::now()).is_none());
        fs::write(&path, "{ not json").unwrap();
        assert!(read_fresh::<Vec<String>>(&path, 300, Utc::now()).is_none());
    }

    #[test]
    fn write_failure_is_a_warning_not_an_error() {
        // A directory where the file should be forces the rename to fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.json");
        fs::create_dir_all(&path).unwrap();
        let warn = write(&path, &1u32, Utc::now());
        assert!(warn.is_some());
        assert!(warn.unwrap().contains("linear cache write failed"));
    }
}
