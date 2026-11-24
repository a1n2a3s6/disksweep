use crate::models::{ArtifactEntry, ArtifactKind, ScanStats, ZombieStatus, ZOMBIE_THRESHOLD_DAYS};
use rayon::prelude::*;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};
use walkdir::WalkDir;

/// Directories that should never be traversed to keep scans fast and OS-safe.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".DS_Store",
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "System Volume Information",
    "Library",
    "Applications",
    ".Trash",
    ".cargo",
    ".rustup",
];

/// Absolute Unix system mount paths to skip when scanning near filesystem root.
const SKIP_UNIX_PREFIXES: &[&str] = &["/proc", "/sys", "/dev", "/run", "/snap", "/boot"];

/// Result of a complete scan operation.
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub entries: Vec<ArtifactEntry>,
    pub stats: ScanStats,
}

/// Scans `root` for developer build/cache artifacts using a non-descending discovery pass
/// followed by parallel Rayon size & file-count aggregation.
pub fn scan_directory(root: &Path) -> ScanResult {
    let start = Instant::now();
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

    // Phase 1: Single-pass non-descending directory discovery.
    // Once a target artifact directory is found, `skip_current_dir()` prevents descending
    // into it, eliminating double-counting of nested `node_modules` or build trees.
    let mut candidates: Vec<(PathBuf, ArtifactKind)> = Vec::new();
    let mut walker = WalkDir::new(&canonical_root)
        .follow_links(false)
        .into_iter();

    while let Some(entry_res) = walker.next() {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_dir() {
            continue;
        }

        if entry.depth() == 0 {
            continue;
        }

        if should_skip_dir(entry.path(), entry.file_name()) {
            walker.skip_current_dir();
            continue;
        }

        if let Some(kind) = ArtifactKind::detect(entry.path()) {
            candidates.push((entry.into_path(), kind));
            walker.skip_current_dir();
        }
    }

    // Phase 2: Parallel Rayon calculation of exact byte size, file count, and project age.
    let now = SystemTime::now();
    let mut entries: Vec<ArtifactEntry> = candidates
        .into_par_iter()
        .map(|(path, kind)| build_artifact_entry(path, kind, now))
        .collect();

    // Sort descending by size (largest space hogs first), tie-breaking by path.
    entries.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });

    let duration_ms = start.elapsed().as_millis();
    let stats = ScanStats::from_entries(canonical_root, &entries, duration_ms);

    ScanResult { entries, stats }
}

/// Computes size, file count, project name, and inactivity age for a single artifact directory.
fn build_artifact_entry(path: PathBuf, kind: ArtifactKind, now: SystemTime) -> ArtifactEntry {
    let (size_bytes, file_count) = calculate_dir_size_and_files(&path);

    let parent_dir = path.parent().unwrap_or(&path);
    let project_name = parent_dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| parent_dir.display().to_string());

    let days_since_modified = calculate_project_age_days(parent_dir, &path, now);
    let is_zombie = days_since_modified >= ZOMBIE_THRESHOLD_DAYS;
    let status = ZombieStatus::from_days(days_since_modified);

    ArtifactEntry {
        path,
        project_name,
        kind,
        size_bytes,
        file_count,
        days_since_modified,
        is_zombie,
        status,
        selected: false,
        deleted: false,
    }
}

/// Recursively sums exact file sizes and counts inside an artifact directory without following symlinks.
fn calculate_dir_size_and_files(dir: &Path) -> (u64, u64) {
    let mut total_bytes = 0u64;
    let mut file_count = 0u64;

    for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
        if entry.file_type().is_file() {
            file_count = file_count.saturating_add(1);
            if let Ok(meta) = entry.metadata() {
                total_bytes = total_bytes.saturating_add(meta.len());
            }
        }
    }

    (total_bytes, file_count)
}

/// Estimates project inactivity age in days by inspecting the parent project directory's
/// top-level entries (manifests, source folders, lockfiles) while skipping the artifact itself.
fn calculate_project_age_days(project_dir: &Path, artifact_path: &Path, now: SystemTime) -> u64 {
    let mut latest_modified: Option<SystemTime> = fs::metadata(project_dir)
        .ok()
        .and_then(|m| m.modified().ok());

    if let Ok(read_dir) = fs::read_dir(project_dir) {
        for entry in read_dir.flatten() {
            let entry_path = entry.path();
            if entry_path == artifact_path {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    latest_modified = match latest_modified {
                        Some(current) if modified > current => Some(modified),
                        None => Some(modified),
                        existing => existing,
                    };
                }
            }
        }
    }

    // Fallback to artifact directory's own mtime if parent metadata could not be read.
    if latest_modified.is_none() {
        latest_modified = fs::metadata(artifact_path)
            .ok()
            .and_then(|m| m.modified().ok());
    }

    match latest_modified {
        Some(mtime) => now
            .duration_since(mtime)
            .map(|d| d.as_secs() / 86_400)
            .unwrap_or(0),
        None => 0,
    }
}

/// Returns true if a directory should be skipped during traversal.
fn should_skip_dir(path: &Path, file_name: &OsStr) -> bool {
    if let Some(name) = file_name.to_str() {
        if SKIP_DIR_NAMES
            .iter()
            .any(|skip| skip.eq_ignore_ascii_case(name))
        {
            return true;
        }
    }

    #[cfg(unix)]
    {
        if let Some(path_str) = path.to_str() {
            if SKIP_UNIX_PREFIXES.iter().any(|prefix| path_str == *prefix) {
                return true;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (path, SKIP_UNIX_PREFIXES);
    }

    false
}
