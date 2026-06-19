//! Canonical claim targets and overlap detection (§S2.6).
//!
//! A [`Target`] is an absolute path canonicalized conservatively: the longest
//! *existing* ancestor is resolved with `realpath` (yielding the authoritative
//! on-disk case and resolving symlinks), the non-existent tail is NFC-normalized
//! and — for ASCII components only — folded to lowercase on **all** volumes so two
//! claims differing only by tail case are treated as overlapping everywhere. A
//! non-ASCII unresolved tail component is rejected (v1 portability restriction);
//! full Unicode case-folding is a later, proven-impl addition.
//!
//! Two targets *overlap* when their normalized segment vectors are equal or one is
//! an ancestor of the other (checked both directions). The same predicate also
//! rejects a target that contains or is contained by the lane root.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::error::LaneError;

/// A canonical claim target plus its normalized path segments (for overlap math).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    normalized: String,
    segments: Vec<String>,
}

impl Target {
    /// The canonical absolute path string (stored into the lock record as `target_normalized`).
    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    /// Build a canonical target from a raw operator string and the canonical lane root.
    ///
    /// Rejects: relative paths; `.`/`..` components; `/`; exactly `$HOME`; and any path
    /// in an ancestor/descendant relationship with `lane_root` (either direction).
    pub fn resolve(raw: &str, home: Option<&str>, lane_root: &Path) -> Result<Self, LaneError> {
        let expanded = expand_home(raw, home);
        let path = PathBuf::from(&expanded);
        if !path.is_absolute() {
            return Err(LaneError::Identity(format!(
                "target must be an absolute path, got {raw}"
            )));
        }
        for c in path.components() {
            if matches!(c, Component::CurDir | Component::ParentDir) {
                return Err(LaneError::Identity(format!(
                    "target must not contain '.' or '..' components: {raw}"
                )));
            }
        }
        let normalized = canonicalize_conservative(&path)?;
        if normalized == "/" {
            return Err(LaneError::Identity("target must not be '/'".into()));
        }
        if let Some(h) = home {
            if let Ok(hc) = std::fs::canonicalize(h) {
                if Path::new(&normalized) == hc.as_path() {
                    return Err(LaneError::Identity(
                        "target must not be exactly $HOME".into(),
                    ));
                }
            }
        }
        let segments = segvec(&normalized);
        let root_segments = segvec(&lane_root.to_string_lossy());
        if overlap_segments(&segments, &root_segments) {
            return Err(LaneError::Identity(
                "target must neither contain nor be contained by the lane root".into(),
            ));
        }
        Ok(Self {
            normalized,
            segments,
        })
    }

    /// Build a [`Target`] from a previously-canonicalized string (a sibling's stored
    /// `target_normalized`), without re-resolving the filesystem. Used by the overlap scan.
    pub fn from_normalized(normalized: &str) -> Self {
        Self {
            segments: segvec(normalized),
            normalized: normalized.to_string(),
        }
    }

    /// True when `self` equals, contains, or is contained by `other`.
    pub fn overlaps(&self, other: &Target) -> bool {
        overlap_segments(&self.segments, &other.segments)
    }
}

/// Expand a leading `~` or `~/...` (and a literal `$HOME` prefix) using `home`.
fn expand_home(raw: &str, home: Option<&str>) -> String {
    let Some(h) = home else {
        return raw.to_string();
    };
    if raw == "~" {
        return h.to_string();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return format!("{}/{}", h.trim_end_matches('/'), rest);
    }
    if raw == "$HOME" {
        return h.to_string();
    }
    if let Some(rest) = raw.strip_prefix("$HOME/") {
        return format!("{}/{}", h.trim_end_matches('/'), rest);
    }
    raw.to_string()
}

/// Canonicalize the longest *existing* ancestor with `realpath`, then re-append the
/// non-existent tail NFC-normalized, ASCII-lowercased, and non-ASCII-rejected.
fn canonicalize_conservative(path: &Path) -> Result<String, LaneError> {
    let mut acc = path.to_path_buf();
    let mut rev_tail: Vec<String> = Vec::new();
    let existing = loop {
        if acc.symlink_metadata().is_ok() {
            break acc.clone();
        }
        match acc.file_name() {
            Some(name) => {
                rev_tail.push(name.to_string_lossy().to_string());
                if !acc.pop() {
                    // Reached a non-existent component with no parent to pop — unreachable
                    // for an absolute path (root always exists), but fail closed if so.
                    return Err(LaneError::Identity(format!(
                        "cannot resolve an existing ancestor for {}",
                        path.display()
                    )));
                }
            }
            None => {
                return Err(LaneError::Identity(format!(
                    "cannot resolve an existing ancestor for {}",
                    path.display()
                )));
            }
        }
    };
    let canon = std::fs::canonicalize(&existing).map_err(LaneError::Io)?;
    let mut out = nfc(&canon.to_string_lossy());

    rev_tail.reverse();
    for comp in rev_tail {
        let nfcd = nfc(&comp);
        if !nfcd.is_ascii() {
            return Err(LaneError::Identity(format!(
                "non-ASCII unresolved target component is not supported in v1: {comp}"
            )));
        }
        let folded = nfcd.to_ascii_lowercase();
        if out == "/" {
            out.push_str(&folded);
        } else {
            out.push('/');
            out.push_str(&folded);
        }
    }
    Ok(out)
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// Split a canonical absolute path into non-empty segments.
fn segvec(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Equal OR ancestor/descendant: the shorter segment vector is a prefix of the longer.
fn overlap_segments(a: &[String], b: &[String]) -> bool {
    let n = a.len().min(b.len());
    n > 0 && a[..n] == b[..n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn overlap_matrix() {
        let a = Target::from_normalized("/a/b");
        let b = Target::from_normalized("/a/b");
        let c = Target::from_normalized("/a/b/c");
        let d = Target::from_normalized("/a/x");
        assert!(a.overlaps(&b)); // equal
        assert!(a.overlaps(&c)); // ancestor
        assert!(c.overlaps(&a)); // descendant
        assert!(!a.overlaps(&d)); // siblings
    }

    #[test]
    fn relative_target_rejected() {
        let r = Target::resolve("rel/path", None, Path::new("/root"));
        assert!(matches!(r, Err(LaneError::Identity(_))));
    }

    #[test]
    fn dotdot_component_rejected() {
        let r = Target::resolve("/a/../b", None, Path::new("/root"));
        assert!(matches!(r, Err(LaneError::Identity(_))));
    }

    #[test]
    fn root_target_rejected() {
        let r = Target::resolve("/", None, Path::new("/root"));
        assert!(matches!(r, Err(LaneError::Identity(_))));
    }

    #[test]
    fn target_inside_lane_root_rejected() {
        // The lane root and a target beneath it overlap → rejected.
        let root = Path::new("/tmp/some-lane-root");
        let segs = segvec("/tmp/some-lane-root/x");
        let root_segs = segvec(&root.to_string_lossy());
        assert!(overlap_segments(&segs, &root_segs));
    }

    #[test]
    fn ascii_tail_is_case_folded() {
        // Two targets differing only by unresolved-tail case normalize identically.
        let root = std::env::temp_dir();
        let a = Target::resolve(
            &format!("{}/UPPER-Tail-XYZ", root.display()),
            None,
            Path::new("/nonexistent-root"),
        )
        .unwrap();
        let b = Target::resolve(
            &format!("{}/upper-tail-xyz", root.display()),
            None,
            Path::new("/nonexistent-root"),
        )
        .unwrap();
        assert_eq!(a.normalized(), b.normalized());
        assert!(a.overlaps(&b));
    }

    #[test]
    fn non_ascii_tail_rejected() {
        let root = std::env::temp_dir();
        let r = Target::resolve(
            &format!("{}/café-tail", root.display()),
            None,
            Path::new("/nonexistent-root"),
        );
        assert!(matches!(r, Err(LaneError::Identity(_))));
    }

    #[test]
    fn home_expansion() {
        assert_eq!(expand_home("~", Some("/Users/x")), "/Users/x");
        assert_eq!(expand_home("~/p", Some("/Users/x")), "/Users/x/p");
        assert_eq!(expand_home("$HOME/p", Some("/Users/x")), "/Users/x/p");
        assert_eq!(expand_home("/abs", Some("/Users/x")), "/abs");
    }
}
