use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Number of days of project inactivity before an artifact is classified as a zombie.
pub const ZOMBIE_THRESHOLD_DAYS: u64 = 30;

/// Supported developer build artifact & dependency bloat directories (9 ecosystems).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Node.js dependency tree (`node_modules`)
    NodeModules,
    /// Rust Cargo build output (`target` with sibling `Cargo.toml`)
    RustTarget,
    /// Python virtual environments & bytecode cache (`.venv`, `venv`, `__pycache__`)
    PythonVenv,
    /// Meta-framework build outputs (`.next`, `.nuxt`, `.svelte-kit`)
    NextBuild,
    /// .NET / C# build outputs (`bin`, `obj` with sibling `*.csproj`)
    DotNetBuild,
    /// Gradle cache & build directories (`.gradle`, or `build` with sibling `build.gradle*`)
    GradleBuild,
    /// Frontend bundler & monorepo caches (`dist`, `.turbo`, `.parcel-cache` with sibling `package.json`)
    DistCache,
    /// Apache Maven build output (`target` with sibling `pom.xml`)
    MavenTarget,
    /// CMake build outputs (`cmake-build-debug`, `cmake-build-release`, or `build` with sibling `CMakeLists.txt`)
    CmakeBuild,
}

impl ArtifactKind {
    /// Inspects a directory path and returns its `ArtifactKind` if it safely matches
    /// a known developer build/cache directory (verifying sibling manifests where required).
    pub fn detect(path: &Path) -> Option<Self> {
        let dir_name = path.file_name()?.to_str()?;
        let parent = path.parent()?;

        match dir_name {
            "node_modules" => Some(Self::NodeModules),
            ".venv" | "venv" | "__pycache__" => Some(Self::PythonVenv),
            ".next" | ".nuxt" | ".svelte-kit" => Some(Self::NextBuild),
            ".gradle" => Some(Self::GradleBuild),
            "cmake-build-debug" | "cmake-build-release" => Some(Self::CmakeBuild),
            "target" => {
                if parent.join("Cargo.toml").is_file() {
                    Some(Self::RustTarget)
                } else if parent.join("pom.xml").is_file() {
                    Some(Self::MavenTarget)
                } else {
                    None
                }
            }
            "bin" | "obj" => {
                if has_sibling_with_extension(parent, "csproj") {
                    Some(Self::DotNetBuild)
                } else {
                    None
                }
            }
            "build" => {
                if has_sibling_with_prefix(parent, "build.gradle") {
                    Some(Self::GradleBuild)
                } else if parent.join("CMakeLists.txt").is_file() {
                    Some(Self::CmakeBuild)
                } else {
                    None
                }
            }
            "dist" | ".turbo" | ".parcel-cache" => {
                if parent.join("package.json").is_file() {
                    Some(Self::DistCache)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Short uppercase pill label displayed in the TUI and CLI table.
    pub fn pill_label(&self) -> &'static str {
        match self {
            Self::NodeModules => "NODE",
            Self::RustTarget => "RUST",
            Self::PythonVenv => "PYTHON",
            Self::NextBuild => "NEXT",
            Self::DotNetBuild => "DOTNET",
            Self::GradleBuild => "BUILD",
            Self::DistCache => "BUILD",
            Self::MavenTarget => "BUILD",
            Self::CmakeBuild => "BUILD",
        }
    }

    /// Searchable lowercase tokens for instant TUI filtering (`/`).
    pub fn filter_tokens(&self) -> &'static str {
        match self {
            Self::NodeModules => "node nodejs javascript js npm yarn pnpm node_modules",
            Self::RustTarget => "rust cargo target rs",
            Self::PythonVenv => "python py venv .venv __pycache__ pip",
            Self::NextBuild => "next nuxt svelte svelte-kit frontend",
            Self::DotNetBuild => "dotnet csharp csproj bin obj net",
            Self::GradleBuild => "gradle java kotlin android build",
            Self::DistCache => "dist turbo parcel cache build bundler js",
            Self::MavenTarget => "maven java pom target build",
            Self::CmakeBuild => "cmake cpp c++ build",
        }
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.pill_label())
    }
}

/// Project activity classification based on `ZOMBIE_THRESHOLD_DAYS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZombieStatus {
    Active,
    Zombie,
}

impl ZombieStatus {
    pub fn from_days(days_since_modified: u64) -> Self {
        if days_since_modified >= ZOMBIE_THRESHOLD_DAYS {
            Self::Zombie
        } else {
            Self::Active
        }
    }
}

/// Represents a single discovered developer artifact directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub path: PathBuf,
    pub project_name: String,
    pub kind: ArtifactKind,
    pub size_bytes: u64,
    pub file_count: u64,
    pub days_since_modified: u64,
    pub is_zombie: bool,
    pub status: ZombieStatus,
    #[serde(skip_serializing_if = "is_false", default)]
    pub selected: bool,
    #[serde(skip_serializing_if = "is_false", default)]
    pub deleted: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Aggregate telemetry for a completed scan pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanStats {
    pub root_path: PathBuf,
    pub total_dirs: usize,
    pub total_files: u64,
    pub total_bytes: u64,
    pub zombie_dirs: usize,
    pub zombie_bytes: u64,
    pub scan_duration_ms: u128,
}

impl ScanStats {
    pub fn from_entries(root_path: PathBuf, entries: &[ArtifactEntry], scan_duration_ms: u128) -> Self {
        let mut total_files = 0u64;
        let mut total_bytes = 0u64;
        let mut zombie_dirs = 0usize;
        let mut zombie_bytes = 0u64;

        for entry in entries {
            total_files = total_files.saturating_add(entry.file_count);
            total_bytes = total_bytes.saturating_add(entry.size_bytes);
            if entry.is_zombie {
                zombie_dirs += 1;
                zombie_bytes = zombie_bytes.saturating_add(entry.size_bytes);
            }
        }

        Self {
            root_path,
            total_dirs: entries.len(),
            total_files,
            total_bytes,
            zombie_dirs,
            zombie_bytes,
            scan_duration_ms,
        }
    }
}

/// Checks if a directory contains at least one file ending with `.ext` (case-insensitive).
fn has_sibling_with_extension(dir: &Path, ext: &str) -> bool {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(file_ext) = path.extension().and_then(|e| e.to_str()) {
                if file_ext.eq_ignore_ascii_case(ext) {
                    return true;
                }
            }
        }
    }
    false
}

/// Checks if a directory contains at least one file whose name starts with `prefix`.
fn has_sibling_with_prefix(dir: &Path, prefix: &str) -> bool {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(prefix) {
                    return true;
                }
            }
        }
    }
    false
}
