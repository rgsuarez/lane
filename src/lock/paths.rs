//! Lane-root resolution, local-filesystem enforcement, the std-only object guard,
//! and the on-disk layout (§S2.3 / §S2.6).
//!
//! [`LaneRoot`] is the only entry point for building state paths. It canonicalizes
//! the longest *existing* ancestor (first-run safe), enforces that the root lives on
//! the same filesystem device as `$HOME` (NFS advisory locks are unreliable), and
//! records the expected owner uid (the home directory's). Symlinks are permitted in
//! the operator-supplied prefix *above* the canonical root (they are resolved by
//! `realpath`); any symlink *beneath* the root is rejected by the object guard.
//!
//! The object guard is std-only (no `O_NOFOLLOW`, no `libc`): for an absent file it
//! uses `create_new`; for an existing file it `symlink_metadata` (lstat) → rejects a
//! symlink/non-regular → `open` → `fstat`s the opened fd and verifies the `(dev, ino)`
//! matches the lstat'd `(dev, ino)` (the TOCTOU guard). Mode policy: a wrong mode is
//! repaired only when the object is same-owner; a wrong owner or wrong object type
//! fails closed (exit 2).

use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::error::LaneError;
use crate::lock::FsOps;

const DIR_MODE: u32 = 0o700;

/// A resolved, local, canonical lane root plus the expected owner uid.
#[derive(Debug, Clone)]
pub struct LaneRoot {
    path: PathBuf,
    canon_existing: PathBuf,
    expected_uid: u32,
}

impl LaneRoot {
    /// The canonical lane-root path (may not yet exist on first run).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The owner uid every state object must have (the home directory's uid).
    pub fn expected_uid(&self) -> u32 {
        self.expected_uid
    }

    pub fn repo_dir(&self, repo: &str) -> PathBuf {
        self.path.join(repo)
    }
    pub fn locks_dir(&self, repo: &str) -> PathBuf {
        self.repo_dir(repo).join("locks")
    }
    pub fn mutexes_dir(&self, repo: &str) -> PathBuf {
        self.repo_dir(repo).join("mutexes")
    }
    pub fn lock_path(&self, repo: &str, lane: &str) -> PathBuf {
        self.locks_dir(repo).join(format!("{lane}.lock"))
    }
    pub fn lane_mutex_path(&self, repo: &str, lane: &str) -> PathBuf {
        self.mutexes_dir(repo).join(format!("{lane}.mutex"))
    }
    pub fn target_mutex_path(&self, repo: &str) -> PathBuf {
        self.mutexes_dir(repo).join("target.mutex")
    }
    pub fn audit_path(&self, repo: &str) -> PathBuf {
        self.repo_dir(repo).join("audit.log")
    }
    pub fn temp_path(&self, repo: &str, lane: &str, token: &str) -> PathBuf {
        self.locks_dir(repo)
            .join(format!("{lane}.lock.{token}.tmp"))
    }

    /// Resolve a raw absolute root path into a canonical, local [`LaneRoot`].
    ///
    /// Fails closed (`NonLocalRoot`, exit 2) when the resolved root's device differs
    /// from `$HOME`'s; `Identity` (exit 2) when the path is not absolute or no existing
    /// ancestor can be resolved.
    pub fn resolve(raw: &Path, home: Option<&str>, fs: &dyn FsOps) -> Result<Self, LaneError> {
        if !raw.is_absolute() {
            return Err(LaneError::Identity(format!(
                "lane root must be an absolute path, got {}",
                raw.display()
            )));
        }
        let home = home.ok_or_else(|| {
            LaneError::Identity("HOME is not set; pass --lane-root explicitly".into())
        })?;
        let home_canon = fs_canonicalize(Path::new(home))?;
        let home_meta = fs::metadata(&home_canon).map_err(LaneError::Io)?;
        let home_dev = home_meta.dev();
        let expected_uid = home_meta.uid();

        // Longest existing ancestor + non-existent tail (first-run safe).
        let mut acc = raw.to_path_buf();
        let mut rev_tail: Vec<PathBuf> = Vec::new();
        let existing = loop {
            if acc.symlink_metadata().is_ok() {
                break acc.clone();
            }
            match acc.file_name() {
                Some(name) => {
                    rev_tail.push(PathBuf::from(name));
                    if !acc.pop() {
                        return Err(LaneError::Identity(format!(
                            "cannot resolve an existing ancestor for {}",
                            raw.display()
                        )));
                    }
                }
                None => {
                    return Err(LaneError::Identity(format!(
                        "cannot resolve an existing ancestor for {}",
                        raw.display()
                    )));
                }
            }
        };
        let canon_existing = fs_canonicalize(&existing)?;

        // Local-FS enforcement: the resolved root's device must equal $HOME's.
        // `device_of` is injectable (the device-id failpoint); the home device is real.
        let root_dev = fs.device_of(&canon_existing).map_err(LaneError::Io)?;
        if root_dev != home_dev {
            return Err(LaneError::NonLocalRoot(format!(
                "resolved root {} is not on the home filesystem",
                canon_existing.display()
            )));
        }

        // Reattach the non-existent tail to the canonical existing ancestor.
        rev_tail.reverse();
        let mut path = canon_existing.clone();
        for c in rev_tail {
            path.push(c);
        }
        Ok(Self {
            path,
            canon_existing,
            expected_uid,
        })
    }

    /// Create `<repo>/`, `<repo>/locks/`, `<repo>/mutexes/` (and any missing root tail),
    /// one component at a time under the object guard. WRITE-PATH ONLY.
    pub fn ensure_write_dirs(&self, repo: &str, fs: &dyn FsOps) -> Result<(), LaneError> {
        // Intermediate components between the existing ancestor and the root: created
        // if missing, but not owner-validated (they may be shared dirs like ~/.config).
        let mut inter: Vec<PathBuf> = Vec::new();
        let mut cur = self.path.parent();
        while let Some(p) = cur {
            if p == self.canon_existing {
                break;
            }
            inter.push(p.to_path_buf());
            cur = p.parent();
        }
        inter.reverse();
        for p in &inter {
            ensure_dir(p, false, fs, self.expected_uid)?;
        }
        // Root and below are our state: owner-validated, mode-repaired.
        ensure_dir(&self.path, true, fs, self.expected_uid)?;
        ensure_dir(&self.repo_dir(repo), true, fs, self.expected_uid)?;
        ensure_dir(&self.locks_dir(repo), true, fs, self.expected_uid)?;
        ensure_dir(&self.mutexes_dir(repo), true, fs, self.expected_uid)?;
        Ok(())
    }
}

/// Resolve a raw root path from flag / `$LANE_ROOT` / `$HOME`-derived default. Absolute-only.
pub fn resolve_raw_root(
    arg: Option<PathBuf>,
    env_val: Option<String>,
    home: Option<&str>,
) -> Result<PathBuf, LaneError> {
    if let Some(p) = arg {
        return require_absolute(p);
    }
    if let Some(v) = env_val {
        if !v.trim().is_empty() {
            return require_absolute(PathBuf::from(v));
        }
    }
    let home =
        home.ok_or_else(|| LaneError::Identity("HOME is not set; pass --lane-root".into()))?;
    require_absolute(PathBuf::from(home).join(".lane"))
}

fn require_absolute(path: PathBuf) -> Result<PathBuf, LaneError> {
    if !path.is_absolute() {
        return Err(LaneError::Identity(format!(
            "lane root must be an absolute path, got {}",
            path.display()
        )));
    }
    Ok(path)
}

fn fs_canonicalize(path: &Path) -> Result<PathBuf, LaneError> {
    std::fs::canonicalize(path).map_err(LaneError::Io)
}

/// Ensure a directory exists with mode 0700, under the object guard. `is_state` toggles
/// owner-validation + mode-repair (true at/below the lane root, false for shared ancestors).
fn ensure_dir(
    path: &Path,
    is_state: bool,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<(), LaneError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => validate_existing_dir(path, &meta, is_state, fs, expected_uid),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Parent must already exist and not be a symlink (callers create top-down).
            if let Some(parent) = path.parent() {
                let pmeta = fs::symlink_metadata(parent).map_err(LaneError::Io)?;
                if pmeta.file_type().is_symlink() {
                    return Err(LaneError::Identity(format!(
                        "parent is a symlink: {}",
                        parent.display()
                    )));
                }
            }
            match fs::create_dir(path) {
                Ok(()) => {
                    fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
                        .map_err(LaneError::Io)?;
                    Ok(())
                }
                // Lost a first-run create race — validate the dir the winner created.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let meta = fs::symlink_metadata(path).map_err(LaneError::Io)?;
                    validate_existing_dir(path, &meta, is_state, fs, expected_uid)
                }
                Err(e) => Err(LaneError::Io(e)),
            }
        }
        Err(e) => Err(LaneError::Io(e)),
    }
}

/// Guarded creation/validation of a single state directory (object guard + owner/mode).
/// Used by audit recovery for `audit.recovered/`; the parent must already exist.
pub(crate) fn ensure_dir_guarded(
    path: &Path,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<(), LaneError> {
    ensure_dir(path, true, fs, expected_uid)
}

/// Validate an existing directory state object: reject symlink/non-directory; for state
/// dirs (at/below the root), fail closed on a wrong owner and repair a same-owner wrong mode.
fn validate_existing_dir(
    path: &Path,
    meta: &fs::Metadata,
    is_state: bool,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<(), LaneError> {
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(LaneError::Identity(format!(
            "symlink where a directory is expected: {}",
            path.display()
        )));
    }
    if !ft.is_dir() {
        return Err(LaneError::Identity(format!(
            "non-directory where a directory is expected: {}",
            path.display()
        )));
    }
    if is_state {
        let owner = fs.owner_uid(path).map_err(LaneError::Io)?;
        if owner != expected_uid {
            return Err(LaneError::Identity(format!(
                "state directory {} has an unexpected owner",
                path.display()
            )));
        }
        let mode = meta.permissions().mode() & 0o7777;
        if mode != DIR_MODE {
            fs::set_permissions(path, Permissions::from_mode(DIR_MODE)).map_err(LaneError::Io)?;
        }
    }
    Ok(())
}

/// Open (creating if absent) a regular file under the object guard, returning a read+write
/// handle. Absent files are created with `create_new` + `mode`; existing files are
/// lstat-checked (reject symlink/non-regular), opened, and fstat-verified `(dev, ino)`
/// (TOCTOU guard). A same-owner wrong mode is repaired; a wrong owner fails closed.
/// WRITE-PATH ONLY — read verbs use the guarded reader (`record::read_guarded`).
pub fn open_or_create_writer(
    path: &Path,
    mode: u32,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<File, LaneError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => open_existing_validated(path, &meta, mode, fs, expected_uid),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(path)
            {
                Ok(f) => Ok(f),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Raced create — validate the now-existing object.
                    let meta = fs::symlink_metadata(path).map_err(LaneError::Io)?;
                    open_existing_validated(path, &meta, mode, fs, expected_uid)
                }
                Err(e) => Err(LaneError::Io(e)),
            }
        }
        Err(e) => Err(LaneError::Io(e)),
    }
}

fn open_existing_validated(
    path: &Path,
    lmeta: &fs::Metadata,
    mode: u32,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<File, LaneError> {
    let ft = lmeta.file_type();
    if ft.is_symlink() {
        return Err(LaneError::Identity(format!(
            "symlink where a regular file is expected: {}",
            path.display()
        )));
    }
    if !ft.is_file() {
        return Err(LaneError::Identity(format!(
            "non-regular file where a regular file is expected: {}",
            path.display()
        )));
    }
    let owner = fs.owner_uid(path).map_err(LaneError::Io)?;
    if owner != expected_uid {
        return Err(LaneError::Identity(format!(
            "state file {} has an unexpected owner",
            path.display()
        )));
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(LaneError::Io)?;
    let fmeta = f.metadata().map_err(LaneError::Io)?;
    if fmeta.dev() != lmeta.dev() || fmeta.ino() != lmeta.ino() {
        return Err(LaneError::Identity(format!(
            "file {} changed between stat and open (possible symlink swap)",
            path.display()
        )));
    }
    let cur = fmeta.permissions().mode() & 0o7777;
    if cur != mode {
        f.set_permissions(Permissions::from_mode(mode))
            .map_err(LaneError::Io)?;
    }
    Ok(f)
}

/// Whether a guarded state-path chain exists on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Every component beneath the root up to the target exists and is valid.
    Present,
    /// A component beneath the root is missing (the path does not exist yet).
    Absent,
}

/// The single read-only state-path ancestor guard (the trust anchor is the canonical
/// `root`). Validates EVERY existing component strictly beneath `root`, up to and
/// including `dir`, with `symlink_metadata` (non-following): each must be a real
/// directory owned by `expected_uid`. Rejects an interior symlink, non-directory, or
/// wrong-owner component (`Identity`, exit 2), and a `dir` not lexically beneath `root`.
/// Returns `Absent` at the first missing component. NEVER mutates (no create/chmod/audit).
///
/// `Path::is_dir`/`metadata`/`canonicalize` are deliberately NOT used here — they follow
/// symlinks and would defeat the guard. Callers building a leaf-file read pass the file's
/// parent directory; the leaf file itself is validated separately by the lstat→open→fstat
/// guard in `record::read_guarded` (the single authoritative read path).
pub fn guard_dir_chain(
    root: &Path,
    dir: &Path,
    fs: &dyn FsOps,
    expected_uid: u32,
) -> Result<Presence, LaneError> {
    let rel = dir.strip_prefix(root).map_err(|_| {
        LaneError::Identity(format!(
            "state path {} is not beneath the lane root",
            dir.display()
        ))
    })?;
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        let name = match comp {
            Component::Normal(n) => n,
            _ => {
                return Err(LaneError::Identity(format!(
                    "unexpected non-normal component in state path {}",
                    dir.display()
                )))
            }
        };
        cur.push(name);
        let meta = match fs::symlink_metadata(&cur) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Presence::Absent),
            Err(e) => return Err(LaneError::Io(e)),
        };
        if meta.file_type().is_symlink() {
            return Err(LaneError::Identity(format!(
                "interior state symlink (refusing to follow): {}",
                cur.display()
            )));
        }
        if !meta.is_dir() {
            return Err(LaneError::Identity(format!(
                "interior state component is not a directory: {}",
                cur.display()
            )));
        }
        let owner = fs.owner_uid(&cur).map_err(LaneError::Io)?;
        if owner != expected_uid {
            return Err(LaneError::Identity(format!(
                "interior state directory {} has an unexpected owner",
                cur.display()
            )));
        }
    }
    Ok(Presence::Present)
}
