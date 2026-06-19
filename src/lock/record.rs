//! The single shared, **object-guarded** claim-record reader used by `board`, `list`,
//! `status`, the overlap scan, and same-lane classification (§S2.12, fail-closed remediation).
//!
//! Read-only and offline. Every claim-state read goes through [`read_guarded`], which:
//! lstat's the path; rejects a symlink or non-regular file; rejects an unexpected owner;
//! opens read-only; and verifies the opened fd's `(dev, ino)` matches the lstat'd
//! `(dev, ino)` (a std-only TOCTOU guard against a symlink swap). It never creates,
//! chmods, audits, or touches mtimes. A genuine transient `NotFound` (a lock unlinked
//! between `read_dir` and open) is `Ok(None)` — skip, not an error. [`read_claim`] adds
//! JSON parsing and the identity guard (`repo`/`lane` must match the on-disk location);
//! both fail closed with a typed [`LaneError`] (exit 2).

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::LaneError;
use crate::lock::FsOps;
use crate::model::ClaimRecord;

/// Guarded read of a claim-state file's raw bytes. The single authoritative read path
/// (no caller may `read_to_string` claim state directly). First validates the directory
/// chain beneath the canonical `root` (rejecting any interior symlink / non-dir /
/// wrong-owner ancestor), then the leaf file. Returns `Ok(None)` for a genuine transient
/// `NotFound` (or a missing ancestor); fails closed (`Identity`) on symlink / non-regular
/// / wrong-owner / TOCTOU `(dev, ino)` mismatch.
pub fn read_guarded(
    path: &Path,
    root: &Path,
    expected_uid: u32,
    fs: &dyn FsOps,
) -> Result<Option<String>, LaneError> {
    // Validate every existing ancestor directory beneath the canonical root.
    if let Some(parent) = path.parent() {
        match crate::lock::paths::guard_dir_chain(root, parent, fs, expected_uid)? {
            crate::lock::paths::Presence::Absent => return Ok(None),
            crate::lock::paths::Presence::Present => {}
        }
    }
    let lmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LaneError::Io(e)),
    };
    let ft = lmeta.file_type();
    if ft.is_symlink() {
        return Err(LaneError::Identity(format!(
            "claim state {} is a symlink (refusing to follow)",
            path.display()
        )));
    }
    if !ft.is_file() {
        return Err(LaneError::Identity(format!(
            "claim state {} is not a regular file",
            path.display()
        )));
    }
    let owner = fs.owner_uid(path).map_err(LaneError::Io)?;
    if owner != expected_uid {
        return Err(LaneError::Identity(format!(
            "claim state {} has an unexpected owner",
            path.display()
        )));
    }
    // Open read-only; a vanish between lstat and open is a benign transient.
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LaneError::Io(e)),
    };
    let fmeta = f.metadata().map_err(LaneError::Io)?;
    if fmeta.dev() != lmeta.dev() || fmeta.ino() != lmeta.ino() {
        return Err(LaneError::Identity(format!(
            "claim state {} changed between stat and open (possible symlink swap)",
            path.display()
        )));
    }
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(LaneError::Io)?;
    Ok(Some(text))
}

/// Guarded read + parse + identity guard. `absent` → `Ok(None)`; unparseable JSON →
/// `Err(Malformed)`; `repo`/`lane` field disagreeing with the on-disk location →
/// `Err(Identity)`; otherwise `Ok(Some(record))`.
pub fn read_claim(
    path: &Path,
    root: &Path,
    expected_uid: u32,
    fs: &dyn FsOps,
) -> Result<Option<ClaimRecord>, LaneError> {
    let Some(text) = read_guarded(path, root, expected_uid, fs)? else {
        return Ok(None);
    };
    let record: ClaimRecord = serde_json::from_str(&text).map_err(|e| LaneError::Malformed {
        path: path.to_path_buf(),
        detail: format!("unparseable claim: {e}"),
    })?;
    check_identity(path, &record)?;
    Ok(Some(record))
}

/// The authoritative-identity guard: the record's `repo`/`lane` must match its on-disk
/// directory and filename stem. Errors name only the filesystem facts, never contents.
pub fn check_identity(path: &Path, record: &ClaimRecord) -> Result<(), LaneError> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let dir_repo = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if record.repo != dir_repo {
        return Err(LaneError::Identity(format!(
            "claim {}: `repo` field does not match its enclosing namespace directory '{dir_repo}'",
            path.display()
        )));
    }
    if record.lane != stem {
        return Err(LaneError::Identity(format!(
            "claim {}: `lane` field does not match its filename stem '{stem}'",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::StdFs;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    fn uid_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().uid()
    }

    fn write(
        dir: &Path,
        repo: &str,
        lane: &str,
        repo_field: &str,
        lane_field: &str,
    ) -> std::path::PathBuf {
        let locks = dir.join(repo).join("locks");
        fs::create_dir_all(&locks).unwrap();
        let body = format!(
            r#"{{"lane":"{lane_field}","repo":"{repo_field}","instance":"i","claimed_at":"2026-06-17T11:00:00Z","updated_at":"2026-06-17T11:00:00Z","expires_at":"2026-06-17T23:00:00Z","ttl_hours":12,"schema_version":1}}"#
        );
        let p = locks.join(format!("{lane}.lock"));
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn absent_is_none_not_error() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("ops/locks/missing.lock");
        assert!(read_claim(&p, dir.path(), uid_of(dir.path()), &StdFs)
            .unwrap()
            .is_none());
    }

    #[test]
    fn well_formed_round_trips() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "ops", "lqos-1", "ops", "lqos-1");
        let rec = read_claim(&p, dir.path(), uid_of(dir.path()), &StdFs)
            .unwrap()
            .unwrap();
        assert_eq!(rec.repo, "ops");
        assert_eq!(rec.lane, "lqos-1");
        assert_eq!(rec.schema_version, Some(1));
    }

    #[test]
    fn malformed_fails_closed() {
        let dir = tempdir().unwrap();
        let locks = dir.path().join("ops/locks");
        fs::create_dir_all(&locks).unwrap();
        let p = locks.join("bad.lock");
        fs::write(&p, "not json").unwrap();
        assert!(matches!(
            read_claim(&p, dir.path(), uid_of(dir.path()), &StdFs),
            Err(LaneError::Malformed { .. })
        ));
    }

    #[test]
    fn repo_mismatch_is_identity_error() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "ops", "lqos-1", "wrong", "lqos-1");
        assert!(matches!(
            read_claim(&p, dir.path(), uid_of(dir.path()), &StdFs),
            Err(LaneError::Identity(_))
        ));
    }

    #[test]
    fn lane_mismatch_is_identity_error() {
        let dir = tempdir().unwrap();
        let p = write(dir.path(), "ops", "lqos-1", "ops", "wrong");
        assert!(matches!(
            read_claim(&p, dir.path(), uid_of(dir.path()), &StdFs),
            Err(LaneError::Identity(_))
        ));
    }

    #[test]
    fn symlinked_claim_fails_closed() {
        let dir = tempdir().unwrap();
        let real = write(dir.path(), "ops", "real", "ops", "real");
        let locks = dir.path().join("ops").join("locks");
        let link = locks.join("linked.lock");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Even though the link target parses, following it is refused.
        assert!(matches!(
            read_guarded(&link, dir.path(), uid_of(dir.path()), &StdFs),
            Err(LaneError::Identity(_))
        ));
    }
}
