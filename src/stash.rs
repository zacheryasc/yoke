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

/// Collect files relative to `dir`. At the loop root only `archive/` is
/// traversed; stash entries and the archive are read recursively.
fn collect_regular_files(
    files: &mut Vec<(String, Vec<u8>)>,
    dir: &Path,
    prefix: &str,
    loop_root: bool,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || (loop_root && name.starts_with("backups-")) {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{}/{}", prefix, name)
            };
            let path = entry.path();
            if path.is_file() {
                if let Ok(contents) = fs::read(&path) {
                    files.push((rel, contents));
                }
            } else if (!loop_root || rel == "archive") && path.is_dir() {
                collect_regular_files(files, &path, &rel, false);
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
fn collect_stashable_files() -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    collect_regular_files(&mut files, Path::new(".loop"), "", true);
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
    let content = fs::read_to_string(index_path).unwrap_or_default();
    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '|');
            Some(StashEntry {
                hash: parts.next()?.to_string(),
                timestamp: parts.next()?.to_string(),
                mode: parts.next()?.to_string(),
                files: parts.next()?.split(',').map(str::to_owned).collect(),
            })
        })
        .collect()
}

// ── Stash core ─────────────────────────────────────────────────────────

/// Snapshot all stashable files in `.loop/` to a new log entry.
/// Returns `Ok(hash)` on success.
fn stash_snapshot(mode: &str, files: &[(String, Vec<u8>)]) -> Result<String, String> {
    if files.is_empty() {
        return Err("no files to stash in .loop/".to_string());
    }

    let now = metrics::unix_now();
    let timestamp = {
        let (y, m, d, h, min, s) = metrics::unix_to_utc(now);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, h, min, s)
    };

    let mut hash = generate_stash_hash(&timestamp, files);

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
        hash = generate_stash_hash(&format!("{}{}", timestamp, attempt + 1), files);
    }

    let entry_dir = stash_base.join(&hash);
    fs::create_dir_all(&entry_dir)
        .map_err(|e| format!("failed to create {}: {}", entry_dir.display(), e))?;

    let file_names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();

    for (name, contents) in files {
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
    stash_snapshot(mode, &files).map_or(0, |_| files.len())
}

/// `yoke stash` — create a new stash entry and print the hash.
pub(crate) fn stash_create(mode: &str) -> i32 {
    if !Path::new(".loop").exists() {
        log_error(".loop/ directory not found");
        return 1;
    }
    match stash_snapshot(mode, &collect_stashable_files()) {
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
    let mut matches = entries
        .iter()
        .filter(|entry| entry.hash.starts_with(target_hash));
    let first = matches.next();
    match matches.count() + usize::from(first.is_some()) {
        0 => {
            log_error(&format!("no stash entry matching '{}'", target_hash));
            None
        }
        1 => first,
        n => {
            log_error(&format!(
                "ambiguous hash '{}' — matches {} entries",
                target_hash, n
            ));
            None
        }
    }
}

/// Auto-stash current state, then replace `.loop/` files with a loaded snapshot.
fn swap_loop_files(restored_files: Vec<(String, Vec<u8>)>, mode: &str) -> Result<(), String> {
    let current_files = collect_stashable_files();
    if !current_files.is_empty() {
        let hash = stash_snapshot(mode, &current_files)?;
        log(&format!(
            "auto-stashed current state → {}{}{}",
            BLUE, hash, RESET
        ));
    }

    for (name, _) in current_files {
        let _ = fs::remove_file(Path::new(".loop").join(name));
    }

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
    if !entry_dir.is_dir() {
        log_error("stash entry is missing or is not a directory");
        return 1;
    }
    let mut restored_files = Vec::new();
    collect_regular_files(&mut restored_files, &entry_dir, "", false);
    restored_files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // Compare the index's comma-separated representation, including names
    // that themselves contain commas.
    if restored_files.is_empty()
        || restored_files
            .iter()
            .flat_map(|(name, _)| name.split(','))
            .ne(target.files.iter().map(String::as_str))
    {
        log_error("stash entry files do not match the index");
        return 1;
    }

    if let Err(msg) = swap_loop_files(restored_files, mode) {
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
        "{ORANGE}{BOLD}[yoke stash]{RESET} snapshot and restore .loop/ state\n\n\
         {BOLD}USAGE:{RESET} yoke stash [subcommand]\n\n\
         {BOLD}SUBCOMMANDS:{RESET}\n  \
         {BOLD}(none){RESET}           Snapshot .loop/ files to stash, then clear the directory\n  \
         {BOLD}log{RESET}              List all stash entries (hash, timestamp, mode, files)\n  \
         {BOLD}checkout <hash>{RESET}  Validate the target snapshot, then auto-stash and restore\n  \
         {BOLD}pop{RESET}              Alias for checkout of the most recent entry\n\n\
         Storage: ~/.yoke/stash/<project-slug>/\n\n\
         {BOLD}EXAMPLES:{RESET}\n  \
         yoke stash                   # snapshot current .loop/ files\n  \
         yoke stash log               # show all stash entries\n  \
         yoke stash checkout a1b2c3d  # restore a specific snapshot\n  \
         yoke stash pop               # restore the most recent snapshot"
    );
}
