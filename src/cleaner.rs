use crate::models::{ArtifactEntry, ArtifactKind};
use rayon::prelude::*;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

/// Deletion mode supported by the cleaning engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanMode {
    /// Permanently removes directories using `fs::remove_dir_all`.
    Permanent,
    /// Simulates deletion and calculates reclaimable bytes without touching disk.
    DryRun,
}

/// Outcome of deleting a single artifact directory.
#[derive(Debug, Clone)]
pub struct ItemCleanResult {
    pub path: PathBuf,
    pub reclaimed_bytes: u64,
    pub success: bool,
    pub error: Option<String>,
}

/// Summary report returned after a batch deletion pass.
#[derive(Debug, Clone)]
pub struct CleanReport {
    pub mode: CleanMode,
    pub deleted_count: usize,
    pub failed_count: usize,
    pub reclaimed_bytes: u64,
    pub duration_ms: u128,
    pub results: Vec<ItemCleanResult>,
}

/// Safely removes all provided artifact entries in parallel using Rayon.
/// Enforces strict path safety guardrails before any filesystem mutation.
pub fn clean_artifacts(targets: &[ArtifactEntry], mode: CleanMode) -> CleanReport {
    let start = Instant::now();

    let results: Vec<ItemCleanResult> = targets
        .par_iter()
        .map(|entry| clean_single_artifact(entry, mode))
        .collect();

    let mut deleted_count = 0usize;
    let mut failed_count = 0usize;
    let mut reclaimed_bytes = 0u64;

    for res in &results {
        if res.success {
            deleted_count += 1;
            reclaimed_bytes = reclaimed_bytes.saturating_add(res.reclaimed_bytes);
        } else {
            failed_count += 1;
        }
    }

    CleanReport {
        mode,
        deleted_count,
        failed_count,
        reclaimed_bytes,
        duration_ms: start.elapsed().as_millis(),
        results,
    }
}

/// Validates safety invariants and removes a single artifact directory (or simulates in DryRun).
fn clean_single_artifact(entry: &ArtifactEntry, mode: CleanMode) -> ItemCleanResult {
    if let Err(reason) = verify_safe_to_delete(&entry.path, entry.kind) {
        return ItemCleanResult {
            path: entry.path.clone(),
            reclaimed_bytes: 0,
            success: false,
            error: Some(reason),
        };
    }

    if mode == CleanMode::DryRun {
        return ItemCleanResult {
            path: entry.path.clone(),
            reclaimed_bytes: entry.size_bytes,
            success: true,
            error: None,
        };
    }

    match fs::remove_dir_all(&entry.path) {
        Ok(()) => ItemCleanResult {
            path: entry.path.clone(),
            reclaimed_bytes: entry.size_bytes,
            success: true,
            error: None,
        },
        Err(err) => ItemCleanResult {
            path: entry.path.clone(),
            reclaimed_bytes: 0,
            success: false,
            error: Some(format!("Failed to delete {}: {}", entry.path.display(), err)),
        },
    }
}

/// Strict multi-layer safety guardrail verifying a path before deletion:
/// 1. Path must exist and be a real directory (never a symlink).
/// 2. Path must never be a filesystem root (`/`, `C:\`, UNC root) or shallow top-level dir.
/// 3. Path must never be the user's home directory or an ancestor of it.
/// 4. Directory name & sibling manifest must still match a recognized `ArtifactKind`.
pub fn verify_safe_to_delete(path: &Path, expected_kind: ArtifactKind) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Refusing to delete empty path".to_string());
    }

    let meta = fs::symlink_metadata(path)
        .map_err(|e| format!("Path no longer accessible ({}): {}", path.display(), e))?;

    if meta.file_type().is_symlink() {
        return Err(format!("Refusing to delete symlink: {}", path.display()));
    }

    if !meta.file_type().is_dir() {
        return Err(format!("Target is not a directory: {}", path.display()));
    }

    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Refuse root paths and single-component paths (e.g. `/`, `C:\`, `/Users`, `C:\Windows`).
    if canonical.parent().is_none() {
        return Err(format!("Refusing to delete filesystem root: {}", canonical.display()));
    }

    let normal_components = canonical
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if normal_components < 2 {
        return Err(format!(
            "Refusing to delete shallow system-level path: {}",
            canonical.display()
        ));
    }

    // Refuse deleting the user's home directory or any direct ancestor of it.
    if let Some(home_dir) = get_home_dir() {
        let canonical_home = fs::canonicalize(&home_dir).unwrap_or(home_dir);
        if canonical == canonical_home || canonical_home.starts_with(&canonical) {
            return Err(format!(
                "Refusing to delete home directory or ancestor: {}",
                canonical.display()
            ));
        }
    }

    // Re-verify that the directory still matches a known artifact kind.
    match ArtifactKind::detect(&canonical) {
        Some(detected) if detected == expected_kind => Ok(()),
        Some(_) => Ok(()),
        None => Err(format!(
            "Safety check failed: '{}' no longer matches a known developer artifact",
            canonical.display()
        )),
    }
}

/// Resolves the current user's home directory cross-platform without extra crate dependencies.
fn get_home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
