//! Read authoritative claim lock records from the local lane root. No network, no git.

use anyhow::Context;
use std::fs;
use std::path::Path;

use crate::model::ClaimRecord;

/// Read all `*.lock` claim records under `lane_root/<repo>/locks/`, optionally
/// filtered to a single repo namespace. A missing lane root yields an empty list.
pub fn read_claims(
    lane_root: &Path,
    repo_filter: Option<&str>,
) -> anyhow::Result<Vec<ClaimRecord>> {
    let mut out: Vec<ClaimRecord> = Vec::new();
    if !lane_root.exists() {
        return Ok(out);
    }

    let repo_dirs = fs::read_dir(lane_root)
        .with_context(|| format!("read lane root {}", lane_root.display()))?;
    for entry in repo_dirs {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let repo_name = entry.file_name().to_string_lossy().to_string();
        if let Some(filter) = repo_filter {
            if filter != repo_name {
                continue;
            }
        }
        let locks_dir = entry.path().join("locks");
        if !locks_dir.is_dir() {
            continue;
        }
        for lock in fs::read_dir(&locks_dir)
            .with_context(|| format!("read locks dir {}", locks_dir.display()))?
        {
            let lock = lock?;
            let path = lock.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            let text =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let record: ClaimRecord = serde_json::from_str(&text)
                .with_context(|| format!("parse claim {}", path.display()))?;
            // Authoritative-identity guard: the record must agree with its location.
            // Errors name the file + expected directory/stem (filesystem facts), never
            // the record's contents.
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            anyhow::ensure!(
                record.repo == repo_name,
                "claim {}: `repo` field does not match its enclosing namespace directory '{}'",
                path.display(),
                repo_name
            );
            anyhow::ensure!(
                record.lane == stem,
                "claim {}: `lane` field does not match its filename stem '{}'",
                path.display(),
                stem
            );
            out.push(record);
        }
    }

    out.sort_by(|a, b| (&a.repo, &a.lane).cmp(&(&b.repo, &b.lane)));
    Ok(out)
}
