//! Read authoritative claim lock records from the local lane root. No network, no git.
//!
//! Slice 2: this goes through the one shared [`crate::lock::record::read_claim`] reader,
//! so `board`, `list`, and `status` agree on identity validation, transient-`NotFound`
//! skipping (a lock unlinked mid-scan is skipped, not an error), and fail-closed
//! handling of malformed/identity-inconsistent records (typed [`LaneError`], exit 2).

use std::fs;
use std::path::Path;

use crate::error::LaneError;
use crate::lock::{record, StdFs};
use crate::model::ClaimRecord;

/// Read all `*.lock` claim records under `lane_root/<repo>/locks/`, optionally filtered
/// to a single repo namespace. A missing lane root yields an empty list. A malformed or
/// identity-inconsistent record fails closed (exit 2); a lock that vanishes mid-scan is
/// skipped.
pub fn read_claims(
    lane_root: &Path,
    repo_filter: Option<&str>,
    expected_uid: u32,
) -> Result<Vec<ClaimRecord>, LaneError> {
    let mut out: Vec<ClaimRecord> = Vec::new();

    let repo_dirs = match fs::read_dir(lane_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(LaneError::Io(e)),
    };
    for entry in repo_dirs {
        let entry = entry.map_err(LaneError::Io)?;
        let ft = entry.file_type().map_err(LaneError::Io)?;
        // An interior symlink directly under the root fails closed; stray non-dirs skip.
        if ft.is_symlink() {
            return Err(LaneError::Identity(format!(
                "interior state symlink (refusing to follow): {}",
                entry.path().display()
            )));
        }
        if !ft.is_dir() {
            continue;
        }
        let repo_name = entry.file_name().to_string_lossy().to_string();
        if let Some(filter) = repo_filter {
            if filter != repo_name {
                continue;
            }
        }
        // Guarded chain (rejects a symlinked repo/locks; never follows). Absent → skip.
        let locks_dir = entry.path().join("locks");
        match crate::lock::paths::guard_dir_chain(lane_root, &locks_dir, &StdFs, expected_uid)? {
            crate::lock::paths::Presence::Absent => continue,
            crate::lock::paths::Presence::Present => {}
        }
        for lock in fs::read_dir(&locks_dir).map_err(LaneError::Io)? {
            let lock = lock.map_err(LaneError::Io)?;
            let path = lock.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            match record::read_claim(&path, lane_root, expected_uid, &StdFs)? {
                Some(rec) => out.push(rec),
                None => continue, // transient NotFound — skip, not an error
            }
        }
    }

    out.sort_by(|a, b| (&a.repo, &a.lane).cmp(&(&b.repo, &b.lane)));
    Ok(out)
}
