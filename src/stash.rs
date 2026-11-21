use std::fs;
use std::path::{Path, PathBuf};

use crate::ansi::{BLUE, BOLD, ORANGE, RESET};
use crate::{log, log_error, metrics};

// ── Stash helpers ──────────────────────────────────────────────────────

/// SipHash timestamp + file names/contents → lower 28 bits → 7-char hex.
fn generate_stash_hash(timestamp: &str, files: &[(String, Vec<u8>)]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    timestamp.hash(&mut hasher);
    for (name, contents) in files {
        name.hash(&mut hasher);
        contents.hash(&mut hasher);
    }
    format!("{:07x}", hasher.finish() & 0x0FFF_FFFF)
}

/// Collect regular files under `dir`, storing paths relative to `.loop/`.
fn collect_regular_files(files: &mut Vec<(String, Vec<u8>)>, dir: &Path, prefix: &str) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{}/{}", prefix, name)
            };
            let path = entry.path();
            if path.is_file() {
                if let Ok(contents) = fs::read(&path) {
                    files.push((rel, contents));
                }
            } else if path.is_dir() {
                collect_regular_files(files, &path, &rel);
            }
        }
    }
}

/// Collect all regular files in `.loop/` excluding dotfile/dotdir entries.
///
/// Top-level loop files are always included. The secondary note archive lives
/// under `.loop/archive/`, so it is included recursively without pulling in
/// unrelated log/checkpoint directories.
/// Returns sorted `(relative path, contents)` pairs for deterministic hashing.
pub(crate) fn collect_stashable_files() -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(".loop") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.starts_with("backups-") {
                continue;
            }
            let path = entry.path();
            if path.is_file() {
                if let Ok(contents) = fs::read(&path) {
                    files.push((name, contents));
                }
            } else if name == "archive" && path.is_dir() {
                collect_regular_files(&mut files, &path, "archive");
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

pub(crate) struct StashEntry {
    pub hash: String,
    pub timestamp: String,
    pub mode: String,
    pub files: Vec<String>,
}

/// Project-scoped stash storage under `~/.yoke/stash/<project-slug>/`.
fn stash_base_dir() -> PathBuf {
    metrics::expand_tilde("~/.yoke/stash").join(metrics::project_slug())
}

/// Parse `~/.yoke/stash/<project-slug>/index` into a list of stash entries (oldest first).
pub(crate) fn parse_stash_index() -> Vec<StashEntry> {
    let index_path = stash_base_dir().join("index");
    let content = match fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() < 4 {
                return None;
            }
            Some(StashEntry {
                hash: parts[0].to_string(),
                timestamp: parts[1].to_string(),
                mode: parts[2].to_string(),
                files: parts[3].split(',').map(|s| s.to_string()).collect(),
            })
        })
        .collect()
}

// ── Stash core ─────────────────────────────────────────────────────────

/// Snapshot all stashable files in `.loop/` to a new log entry.
/// Returns `Ok(hash)` on success.
pub(crate) fn stash_snapshot(mode: &str) -> Result<String, String> {
    let files = collect_stashable_files();
    if files.is_empty() {
        return Err("no files to stash in .loop/".to_string());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let timestamp = {
        let (y, m, d, h, min, s) = metrics::unix_to_utc(now);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, h, min, s)
    };

    let mut hash = generate_stash_hash(&timestamp, &files);

    // Collision handling: rehash with counter suffix
    let stash_base = stash_base_dir();
    fs::create_dir_all(&stash_base)
        .map_err(|e| format!("failed to create {}: {}", stash_base.display(), e))?;
    for attempt in 0..100u32 {
        if !stash_base.join(&hash).exists() {
            break;
        }
        if attempt == 99 {
            return Err("hash collision after 100 attempts".to_string());
        }
        hash = generate_stash_hash(&format!("{}{}", timestamp, attempt + 1), &files);
    }

    let entry_dir = stash_base.join(&hash);
    fs::create_dir_all(&entry_dir)
        .map_err(|e| format!("failed to create {}: {}", entry_dir.display(), e))?;

    let file_names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();

    for (name, contents) in &files {
        let dest = entry_dir.join(name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
        fs::write(&dest, contents)
            .map_err(|e| format!("failed to write {}: {}", dest.display(), e))?;
    }

    // Append to index
    let index_path = stash_base.join("index");
    let line = format!("{}|{}|{}|{}\n", hash, timestamp, mode, file_names.join(","));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index_path)
        .map_err(|e| format!("failed to open {}: {}", index_path.display(), e))?;
    std::io::Write::write_all(&mut f, line.as_bytes())
        .map_err(|e| format!("failed to write index: {}", e))?;

    Ok(hash)
}

// ── Stash commands ─────────────────────────────────────────────────────

/// Snapshot `.loop/` to a new stash log entry. Returns file count for `clean()`.
pub(crate) fn stash_working_files(mode: &str) -> usize {
    let files = collect_stashable_files();
    if files.is_empty() {
        return 0;
    }
    let count = files.len();
    match stash_snapshot(mode) {
        Ok(_) => count,
        Err(_) => 0,
    }
}

/// `yoke stash` — create a new stash entry and print the hash.
pub(crate) fn stash_create(mode: &str) -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    match stash_snapshot(mode) {
        Ok(hash) => {
            for (name, _) in &collect_stashable_files() {
                let _ = fs::remove_file(Path::new(".loop").join(name));
            }
            log(&format!("stashed → {}{}{}", BLUE, hash, RESET));
            0
        }
        Err(msg) => {
            log_error(&msg);
            1
        }
    }
}

/// `yoke stash log` — list all stash entries (newest first).
pub(crate) fn stash_log_cmd() -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    let entries = parse_stash_index();
    if entries.is_empty() {
        log("no stash entries");
        return 0;
    }
    for entry in entries.iter().rev() {
        eprintln!(
            "{}{}{}{}  {}  mode={}",
            BOLD, BLUE, entry.hash, RESET, entry.timestamp, entry.mode
        );
        for file in &entry.files {
            eprintln!("    {}", file);
        }
    }
    0
}

/// Resolve a hash prefix to a single stash entry. Returns the entry or logs an error.
fn resolve_stash_entry<'a>(entries: &'a [StashEntry], target_hash: &str) -> Option<&'a StashEntry> {
    let matches: Vec<&StashEntry> = entries
        .iter()
        .filter(|e| e.hash == target_hash || e.hash.starts_with(target_hash))
        .collect();

    match matches.len() {
        0 => {
            log_error(&format!("no stash entry matching '{}'", target_hash));
            None
        }
        1 => Some(matches[0]),
        n => {
            log_error(&format!(
                "ambiguous hash '{}' — matches {} entries",
                target_hash, n
            ));
            None
        }
    }
}

/// Auto-stash current state, then replace `.loop/` files with those from `entry_dir`.
fn swap_loop_files(entry_dir: &Path, mode: &str) -> Result<(), String> {
    let current_files = collect_stashable_files();
    if !current_files.is_empty() {
        let hash = stash_snapshot(mode)?;
        log(&format!(
            "auto-stashed current state → {}{}{}",
            BLUE, hash, RESET
        ));
    }

    for (name, _) in &collect_stashable_files() {
        let _ = fs::remove_file(Path::new(".loop").join(name));
    }

    let mut restored_files = Vec::new();
    collect_regular_files(&mut restored_files, entry_dir, "");
    for (name, contents) in restored_files {
        let dest = Path::new(".loop").join(&name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
        fs::write(&dest, contents).map_err(|e| format!("failed to restore {}: {}", name, e))?;
    }
    Ok(())
}

/// `yoke stash checkout <hash>` — auto-stash current state, clear `.loop/` files,
/// restore target entry. Supports prefix matching.
pub(crate) fn stash_checkout(target_hash: &str, mode: &str) -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    let entries = parse_stash_index();
    if entries.is_empty() {
        log_error("no stash entries");
        return 1;
    }

    let target = match resolve_stash_entry(&entries, target_hash) {
        Some(t) => t,
        None => return 1,
    };

    let entry_dir = stash_base_dir().join(&target.hash);
    if !entry_dir.exists() {
        log_error("stash entry directory missing — index is corrupt");
        return 1;
    }

    if let Err(msg) = swap_loop_files(&entry_dir, mode) {
        log_error(&msg);
        return 1;
    }

    log(&format!("checked out {}{}{}", BLUE, target.hash, RESET));
    0
}

/// `yoke stash pop` — checkout the most recent stash entry.
pub(crate) fn stash_pop(mode: &str) -> i32 {
    let entries = parse_stash_index();
    match entries.last() {
        Some(entry) => stash_checkout(&entry.hash, mode),
        None => {
            log_error("no stash found — nothing to restore");
            1
        }
    }
}

pub(crate) fn print_stash_help() {
    eprintln!(
        "{}{}[yoke stash]{} snapshot and restore .loop/ state",
        ORANGE, BOLD, RESET
    );
    eprintln!();
    eprintln!("{}USAGE:{} yoke stash [subcommand]", BOLD, RESET);
    eprintln!();
    eprintln!("{}SUBCOMMANDS:{}", BOLD, RESET);
    eprintln!(
        "  {}(none){}           Snapshot .loop/ files to stash, then clear the directory",
        BOLD, RESET
    );
    eprintln!(
        "  {}log{}              List all stash entries (hash, timestamp, mode, files)",
        BOLD, RESET
    );
    eprintln!(
        "  {}checkout <hash>{}  Auto-stash current state, then restore target entry",
        BOLD, RESET
    );
    eprintln!(
        "  {}pop{}              Alias for checkout of the most recent entry",
        BOLD, RESET
    );
    eprintln!();
    eprintln!("Storage: ~/.yoke/stash/<project-slug>/");
    eprintln!();
    eprintln!("{}EXAMPLES:{}", BOLD, RESET);
    eprintln!("  yoke stash                   # snapshot current .loop/ files");
    eprintln!("  yoke stash log               # show all stash entries");
    eprintln!("  yoke stash checkout a1b2c3d  # restore a specific snapshot");
    eprintln!("  yoke stash pop               # restore the most recent snapshot");
}
