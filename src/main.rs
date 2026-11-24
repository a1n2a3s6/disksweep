mod cleaner;
mod models;
mod scanner;
mod tui;

use clap::{Parser, Subcommand};
use cleaner::{clean_artifacts, CleanMode};
use humansize::{format_size, DECIMAL};
use models::{ArtifactEntry, ScanStats};
use scanner::scan_directory;
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

#[derive(Parser, Debug)]
#[command(
    name = "disksweep",
    version,
    about = "Blazing-fast cross-platform developer disk cleaner & interactive TUI",
    long_about = "DiskSweep detects and safely reclaims gigabytes of developer build artifacts (node_modules, Rust target, Python .venv, .next, .NET bin/obj, Gradle, Maven, CMake, dist) across Windows, Linux, and macOS."
)]
struct Cli {
    /// Directory to scan in the interactive TUI (defaults to current directory)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Scan a directory and print an aligned summary table or machine-readable JSON
    Scan {
        /// Target directory to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Only include artifacts from projects untouched for at least N days
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,

        /// Output machine-readable JSON for scripts & CI pipelines
        #[arg(long)]
        json: bool,
    },
    /// Headless batch cleanup of matching developer artifacts
    Clean {
        /// Target directory to scan (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Minimum project inactivity age in days required before deletion
        #[arg(long, value_name = "DAYS")]
        older_than: u64,

        /// Simulate deletion without removing any files
        #[arg(long)]
        dry_run: bool,

        /// Skip confirmation prompt (for scripts & automation)
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Serialize)]
struct JsonScanOutput {
    stats: ScanStats,
    artifacts: Vec<ArtifactEntry>,
}

fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Some(Commands::Scan {
            path,
            older_than,
            json,
        }) => run_scan(&path, older_than, json),
        Some(Commands::Clean {
            path,
            older_than,
            dry_run,
            yes,
        }) => run_clean(&path, older_than, dry_run, yes),
        None => {
            let target = cli.path.unwrap_or_else(|| PathBuf::from("."));
            if !target.exists() {
                eprintln!("Error: path '{}' does not exist.", target.display());
                process::exit(1);
            }
            if let Err(err) = tui::run_tui(&target) {
                eprintln!("TUI error: {}", err);
                1
            } else {
                0
            }
        }
    };

    if exit_code != 0 {
        process::exit(exit_code);
    }
}

fn run_scan(path: &Path, older_than: Option<u64>, json: bool) -> i32 {
    if !path.exists() {
        eprintln!("Error: path '{}' does not exist.", path.display());
        return 1;
    }

    let scan = scan_directory(path);
    let filtered: Vec<ArtifactEntry> = scan
        .entries
        .into_iter()
        .filter(|e| older_than.map_or(true, |days| e.days_since_modified >= days))
        .collect();

    let stats = ScanStats::from_entries(scan.stats.root_path, &filtered, scan.stats.scan_duration_ms);

    if json {
        let payload = JsonScanOutput {
            stats,
            artifacts: filtered,
        };
        match serde_json::to_string_pretty(&payload) {
            Ok(serialized) => println!("{}", serialized),
            Err(err) => {
                eprintln!("JSON serialization error: {}", err);
                return 1;
            }
        }
        return 0;
    }

    println!(
        "DISKSWEEP v{} — Scanned {} in {} ms",
        env!("CARGO_PKG_VERSION"),
        stats.root_path.display(),
        stats.scan_duration_ms
    );
    println!(
        "Found {} across {} dirs ({} zombie dirs = {})\n",
        format_size(stats.total_bytes, DECIMAL),
        stats.total_dirs,
        stats.zombie_dirs,
        format_size(stats.zombie_bytes, DECIMAL)
    );

    if filtered.is_empty() {
        println!("No matching developer artifacts found.");
        return 0;
    }

    println!(
        "{:>10}  {:>7}  {:>7}  {:<22}  {}",
        "SIZE", "TYPE", "AGE", "PROJECT", "PATH"
    );
    println!("{}", "-".repeat(84));

    for entry in &filtered {
        let age_tag = if entry.is_zombie {
            format!("{}d*", entry.days_since_modified)
        } else {
            format!("{}d", entry.days_since_modified)
        };
        println!(
            "{:>10}  {:>7}  {:>7}  {:<22}  {}",
            format_size(entry.size_bytes, DECIMAL),
            entry.kind.pill_label(),
            age_tag,
            truncate_right(&entry.project_name, 22),
            entry.path.display()
        );
    }

    println!("\n* = Zombie project (untouched for >= 30 days)");
    0
}

fn run_clean(path: &Path, older_than: u64, dry_run: bool, yes: bool) -> i32 {
    if !path.exists() {
        eprintln!("Error: path '{}' does not exist.", path.display());
        return 1;
    }

    let scan = scan_directory(path);
    let targets: Vec<ArtifactEntry> = scan
        .entries
        .into_iter()
        .filter(|e| e.days_since_modified >= older_than)
        .collect();

    if targets.is_empty() {
        println!(
            "No artifacts found in '{}' older than {} days.",
            scan.stats.root_path.display(),
            older_than
        );
        return 0;
    }

    let total_bytes: u64 = targets.iter().map(|e| e.size_bytes).sum();
    let mode_label = if dry_run { "DRY RUN" } else { "PERMANENT" };

    println!(
        "DISKSWEEP CLEAN [{}] — {} directories ({}) older than {} days:",
        mode_label,
        targets.len(),
        format_size(total_bytes, DECIMAL),
        older_than
    );
    for entry in &targets {
        println!(
            "  [{:>6}] {:>10}  ({:>3}d)  {}",
            entry.kind.pill_label(),
            format_size(entry.size_bytes, DECIMAL),
            entry.days_since_modified,
            entry.path.display()
        );
    }

    if !dry_run && !yes {
        print!(
            "\nProceed to permanently delete {} directories ({})? [y/N]: ",
            targets.len(),
            format_size(total_bytes, DECIMAL)
        );
        let _ = io::stdout().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            eprintln!("Aborted: unable to read confirmation input.");
            return 1;
        }
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("Aborted. No files were removed.");
            return 0;
        }
    }

    let mode = if dry_run {
        CleanMode::DryRun
    } else {
        CleanMode::Permanent
    };
    let report = clean_artifacts(&targets, mode);

    if dry_run {
        println!(
            "\n[DRY RUN] Would reclaim {} across {} directories (checked in {} ms).",
            format_size(report.reclaimed_bytes, DECIMAL),
            report.deleted_count,
            report.duration_ms
        );
    } else {
        println!(
            "\nReclaimed {} across {} directories in {} ms ({} failed).",
            format_size(report.reclaimed_bytes, DECIMAL),
            report.deleted_count,
            report.duration_ms,
            report.failed_count
        );
    }

    for item in &report.results {
        if let Some(err) = &item.error {
            eprintln!("  [WARN] {}", err);
        }
    }

    if report.failed_count > 0 { 1 } else { 0 }
}

fn truncate_right(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let head: String = chars[..keep].iter().collect();
    format!("{}…", head)
}
