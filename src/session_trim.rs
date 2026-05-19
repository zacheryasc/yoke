//! Session-JSONL trimming for Claude Code prompt-cache reuse across rounds.
//!
//! Yoke uses `claude --resume <sid>` to carry a worker's conversation across
//! iterations so Anthropic's prefix cache stays warm. Naive resume grows the
//! session monotonically — bash outputs, thinking blocks, intermediate Reads
//! that are no longer relevant — all of it stays in the prefix and is paid
//! for at cache-read rates every round.
//!
//! This module trims the session file between rounds. The agent declares a
//! `KEEP: <path> <path> ...` line in `.loop/notes.md` listing the file Reads
//! whose results should stay in conversation history. Everything else is
//! dropped, the parent-uuid chain is re-linked across the gaps, and the file
//! is atomically rewritten in place.
//!
//! Safety: if the trim's own validation fails (broken parent chain, orphaned
//! tool_use without tool_result, parse error), the original session is kept
//! untouched and we log a warning. `YOKE_DISABLE_SESSION_TRIM=1` skips the
//! whole pass.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Default)]
pub struct TrimStats {
    pub records_total: usize,
    pub records_kept: usize,
    pub records_dropped: usize,
    pub keep_paths: Vec<String>,
    pub skipped: bool,
}

/// Parse a `KEEP:` line from notes.md (or any text). Returns absolute paths
/// resolved against `cwd`. Multiple `KEEP:` lines are unioned. Missing or
/// `*` token is treated as "keep nothing" — the caller decides what that
/// means, but this fn just returns the explicit paths.
pub fn parse_keep_list(notes_text: &str, cwd: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for line in notes_text.lines() {
        let trimmed = line.trim_start();
        let rest = match trimmed.strip_prefix("KEEP:") {
            Some(r) => r,
            None => continue,
        };
        for tok in rest.split_whitespace() {
            if tok == "*" {
                continue;
            }
            let p = PathBuf::from(tok);
            let abs = if p.is_absolute() { p } else { cwd.join(p) };
            // Best-effort canonicalize so symlinks / .. don't cause mismatches.
            let final_path = fs::canonicalize(&abs).unwrap_or(abs);
            out.insert(final_path);
        }
    }
    out
}

/// Locate Claude Code's session file on disk.
/// Format: `<home>/.claude/projects/<cwd-with-/-replaced-by->/<sid>.jsonl`.
pub fn session_file_path(home: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    let cwd_str = cwd.to_string_lossy();
    // Claude Code's slug: replace `/` with `-`. A leading slash becomes a
    // leading dash. `.` characters in path components are preserved.
    let slug = cwd_str.replace('/', "-");
    home.join(".claude")
        .join("projects")
        .join(slug)
        .join(format!("{}.jsonl", session_id))
}

/// Trim a session file in place. Returns stats. If the trim aborts safely
/// (escape hatch / no kept content / validation failure), the original file
/// is untouched.
pub fn trim_session(session_path: &Path, keep_paths: &HashSet<PathBuf>) -> Result<TrimStats, String> {
    let mut stats = TrimStats {
        keep_paths: keep_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        ..TrimStats::default()
    };

    if std::env::var_os("YOKE_DISABLE_SESSION_TRIM").is_some() {
        stats.skipped = true;
        return Ok(stats);
    }
    if !session_path.exists() {
        return Err(format!("session file not found: {}", session_path.display()));
    }

    let content = fs::read_to_string(session_path)
        .map_err(|e| format!("read {}: {}", session_path.display(), e))?;
    let raw_lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    stats.records_total = raw_lines.len();

    let mut records: Vec<Value> = Vec::with_capacity(raw_lines.len());
    for (i, line) in raw_lines.iter().enumerate() {
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("parse line {}: {}", i + 1, e))?;
        records.push(v);
    }

    let decisions = classify(&records, keep_paths);

    let trimmed = relink_and_emit(&records, &raw_lines, &decisions)?;
    stats.records_kept = trimmed.lines().filter(|l| !l.trim().is_empty()).count();
    stats.records_dropped = stats.records_total.saturating_sub(stats.records_kept);

    validate(&trimmed)?;

    // Atomic write: tmp → rename. Keep one .bak for recovery / debugging.
    let bak_path = session_path.with_extension("jsonl.bak");
    let _ = fs::copy(session_path, &bak_path);
    let tmp_path = session_path.with_extension("jsonl.tmp");
    {
        let mut f = fs::File::create(&tmp_path)
            .map_err(|e| format!("create tmp: {}", e))?;
        f.write_all(trimmed.as_bytes())
            .map_err(|e| format!("write tmp: {}", e))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp_path, session_path)
        .map_err(|e| format!("rename: {}", e))?;

    Ok(stats)
}

/// Per-record decision: keep as-is, drop entirely, or keep with a rewritten
/// parentUuid.
#[derive(Debug, Clone)]
enum Decision {
    Keep,
    Drop,
}

fn classify(records: &[Value], keep_paths: &HashSet<PathBuf>) -> Vec<Decision> {
    // Two-pass: first identify which tool_use ids we keep, then decide each record.
    let mut kept_tool_use_ids: HashSet<String> = HashSet::new();
    for r in records {
        if !is_assistant(r) {
            continue;
        }
        let Some(block) = first_content_block(r) else { continue };
        if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
            continue;
        }
        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name != "Read" {
            continue;
        }
        let path_str = block
            .get("input")
            .and_then(|v| v.get("file_path"))
            .and_then(|v| v.as_str());
        let Some(path_str) = path_str else { continue };
        let p = PathBuf::from(path_str);
        let canonical = fs::canonicalize(&p).unwrap_or(p);
        if keep_paths.contains(&canonical)
            && let Some(id) = block.get("id").and_then(|v| v.as_str())
        {
            kept_tool_use_ids.insert(id.to_string());
        }
    }

    records
        .iter()
        .map(|r| decide(r, &kept_tool_use_ids))
        .collect()
}

fn decide(record: &Value, kept_tool_use_ids: &HashSet<String>) -> Decision {
    // Metadata records (no top-level uuid OR parentUuid is absent and type is bookkeeping):
    // always keep.
    let typ = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match typ {
        // Non-conversation bookkeeping — keep unchanged.
        "permission-mode"
        | "file-history-snapshot"
        | "queue-operation"
        | "ai-title"
        | "last-prompt"
        | "attachment" => return Decision::Keep,
        _ => {}
    }

    // user / assistant: examine content.
    let Some(msg) = record.get("message") else {
        return Decision::Keep;
    };
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

    // Initial user prompt: content is a plain string, not an array. Always keep.
    if role == "user" {
        match msg.get("content") {
            Some(Value::String(_)) => return Decision::Keep,
            Some(Value::Array(arr)) => {
                // tool_result wrapper. Keep only if its tool_use_id was kept.
                if arr.is_empty() {
                    return Decision::Keep;
                }
                let block = &arr[0];
                let btyp = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if btyp == "tool_result" {
                    let id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    if kept_tool_use_ids.contains(id) {
                        return Decision::Keep;
                    } else {
                        return Decision::Drop;
                    }
                }
                // Other user content (unusual): keep defensively.
                return Decision::Keep;
            }
            _ => return Decision::Keep,
        }
    }

    if role == "assistant" {
        let Some(block) = first_content_block(record) else {
            return Decision::Drop;
        };
        let btyp = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match btyp {
            "thinking" => Decision::Drop,
            "text" => Decision::Drop,
            "tool_use" => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if kept_tool_use_ids.contains(id) {
                    Decision::Keep
                } else {
                    Decision::Drop
                }
            }
            _ => Decision::Drop,
        }
    } else {
        // Unknown role — keep, don't make things worse.
        Decision::Keep
    }
}

fn is_assistant(record: &Value) -> bool {
    record.get("type").and_then(|v| v.as_str()) == Some("assistant")
}

fn first_content_block(record: &Value) -> Option<&Value> {
    record
        .get("message")?
        .get("content")?
        .as_array()?
        .first()
}

/// Build the trimmed JSONL output. For surviving records whose parentUuid
/// points to a dropped record, walks up the parent chain to find the nearest
/// surviving ancestor and rewrites the field.
fn relink_and_emit(records: &[Value], raw: &[&str], decisions: &[Decision]) -> Result<String, String> {
    // uuid → parentUuid index for ALL records that have a uuid. Used to walk
    // up the chain when re-linking.
    let mut parent_of: HashMap<String, Option<String>> = HashMap::new();
    let mut kept_uuids: HashSet<String> = HashSet::new();
    for (i, r) in records.iter().enumerate() {
        let Some(uuid) = r.get("uuid").and_then(|v| v.as_str()) else { continue };
        let parent = r
            .get("parentUuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        parent_of.insert(uuid.to_string(), parent);
        if matches!(decisions[i], Decision::Keep) {
            kept_uuids.insert(uuid.to_string());
        }
    }

    // For each kept uuid, compute the rewritten parentUuid (nearest kept
    // ancestor or null).
    let mut rewritten_parent: HashMap<String, Option<String>> = HashMap::new();
    for uuid in &kept_uuids {
        let mut cur = parent_of.get(uuid).cloned().flatten();
        while let Some(p) = cur {
            if kept_uuids.contains(&p) {
                rewritten_parent.insert(uuid.clone(), Some(p));
                break;
            }
            cur = parent_of.get(&p).cloned().flatten();
        }
        if !rewritten_parent.contains_key(uuid) {
            rewritten_parent.insert(uuid.clone(), None);
        }
    }

    let mut out = String::with_capacity(raw.iter().map(|l| l.len() + 1).sum());
    for (i, r) in records.iter().enumerate() {
        if matches!(decisions[i], Decision::Drop) {
            continue;
        }
        let uuid_opt = r.get("uuid").and_then(|v| v.as_str()).map(|s| s.to_string());
        // If this record has a uuid AND its rewritten parent differs from
        // the on-disk parent, re-serialize. Otherwise emit raw.
        let needs_rewrite = match &uuid_opt {
            Some(uuid) => {
                let orig = parent_of.get(uuid).cloned().flatten();
                let new = rewritten_parent.get(uuid).cloned().flatten();
                orig != new
            }
            None => false,
        };
        if needs_rewrite {
            let mut v = r.clone();
            let uuid = uuid_opt.unwrap();
            let new_parent = rewritten_parent.get(&uuid).cloned().flatten();
            if let Some(obj) = v.as_object_mut() {
                match new_parent {
                    Some(p) => {
                        obj.insert("parentUuid".to_string(), Value::String(p));
                    }
                    None => {
                        obj.insert("parentUuid".to_string(), Value::Null);
                    }
                }
            }
            let s = serde_json::to_string(&v)
                .map_err(|e| format!("serialize: {}", e))?;
            out.push_str(&s);
            out.push('\n');
        } else {
            out.push_str(raw[i]);
            out.push('\n');
        }
    }

    Ok(out)
}

/// Validate that the trimmed JSONL is internally consistent:
/// every tool_use has a matching tool_result downstream.
fn validate(trimmed: &str) -> Result<(), String> {
    let mut tool_use_ids: HashSet<String> = HashSet::new();
    let mut tool_result_ids: HashSet<String> = HashSet::new();
    for (i, line) in trimmed.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("validate parse line {}: {}", i + 1, e))?;
        let Some(arr) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else {
            continue;
        };
        for block in arr {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                        tool_use_ids.insert(id.to_string());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                        tool_result_ids.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    for id in &tool_use_ids {
        if !tool_result_ids.contains(id) {
            return Err(format!("orphan tool_use {}: no matching tool_result", id));
        }
    }
    for id in &tool_result_ids {
        if !tool_use_ids.contains(id) {
            return Err(format!("orphan tool_result {}: no matching tool_use", id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Serializes tests that read/mutate the `YOKE_DISABLE_SESSION_TRIM` env
    /// var. cargo runs tests in parallel within a binary; without this they
    /// race on a process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Clear the env var before running the closure, then drop the guard.
    fn with_clean_env<F: FnOnce()>(f: F) {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: tests are serialized via ENV_LOCK; no other thread will
        // observe a partial write to environ.
        unsafe { std::env::remove_var("YOKE_DISABLE_SESSION_TRIM") };
        f();
    }

    #[test]
    fn parse_keep_list_basic() {
        let cwd = PathBuf::from("/tmp");
        let s = "STATUS: IN_PROGRESS\nKEEP: src/a.rs /abs/b.rs\nfoo\n";
        let got = parse_keep_list(s, &cwd);
        assert!(got.iter().any(|p| p.ends_with("a.rs")));
        assert!(got.iter().any(|p| p == &PathBuf::from("/abs/b.rs")));
    }

    #[test]
    fn parse_keep_list_missing() {
        let cwd = PathBuf::from("/tmp");
        let s = "STATUS: DONE\n";
        assert!(parse_keep_list(s, &cwd).is_empty());
    }

    #[test]
    fn parse_keep_list_star_means_nothing() {
        let cwd = PathBuf::from("/tmp");
        let s = "KEEP: *\n";
        assert!(parse_keep_list(s, &cwd).is_empty());
    }

    #[test]
    fn session_file_path_slug() {
        let p = session_file_path(
            Path::new("/home/u"),
            Path::new("/workspace"),
            "abc-123",
        );
        assert_eq!(p, PathBuf::from("/home/u/.claude/projects/-workspace/abc-123.jsonl"));
    }

    /// Tiny realistic session: bootstrap + initial prompt + 2 Reads + a Bash
    /// + a thinking block. Used by scenario tests below.
    fn write_fixture(dir: &Path, paths: &[&str]) -> PathBuf {
        let mut lines: Vec<String> = Vec::new();
        // queue-operation (metadata) — has no uuid/parentUuid.
        lines.push(r#"{"type":"queue-operation","sessionId":"s1"}"#.to_string());
        // initial user prompt (root of conversation)
        lines.push(r#"{"type":"user","uuid":"u-root","parentUuid":null,"message":{"role":"user","content":"Read .loop/protocol.md and follow its instructions."}}"#.to_string());
        // assistant thinking — should always be dropped
        lines.push(r#"{"type":"assistant","uuid":"u-think","parentUuid":"u-root","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."}]}}"#.to_string());
        // Read tool_use for paths[0]
        let p0 = paths.first().copied().unwrap_or("/tmp/a.rs");
        lines.push(format!(
            r#"{{"type":"assistant","uuid":"u-read-a","parentUuid":"u-think","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu-a","name":"Read","input":{{"file_path":"{}"}}}}]}}}}"#,
            p0
        ));
        // tool_result for the Read
        lines.push(r#"{"type":"user","uuid":"u-res-a","parentUuid":"u-read-a","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-a","content":"file A contents"}]}}"#.to_string());
        // Bash tool_use — should always be dropped
        lines.push(r#"{"type":"assistant","uuid":"u-bash","parentUuid":"u-res-a","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu-bash","name":"Bash","input":{"command":"ls"}}]}}"#.to_string());
        lines.push(r#"{"type":"user","uuid":"u-res-bash","parentUuid":"u-bash","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-bash","content":"a\nb\nc"}]}}"#.to_string());
        // Read tool_use for paths[1] (a second file)
        let p1 = paths.get(1).copied().unwrap_or("/tmp/b.rs");
        lines.push(format!(
            r#"{{"type":"assistant","uuid":"u-read-b","parentUuid":"u-res-bash","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu-b","name":"Read","input":{{"file_path":"{}"}}}}]}}}}"#,
            p1
        ));
        lines.push(r#"{"type":"user","uuid":"u-res-b","parentUuid":"u-read-b","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu-b","content":"file B contents"}]}}"#.to_string());

        let path = dir.join("s1.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for l in &lines {
            writeln!(f, "{}", l).unwrap();
        }
        path
    }

    fn read_records(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn trim_with_one_keep_drops_other_reads_and_bash_and_thinking() {
        let tmp = tempfile::tempdir().unwrap();
        let a_path = tmp.path().join("a.rs");
        let b_path = tmp.path().join("b.rs");
        fs::write(&a_path, "fn a(){}").unwrap();
        fs::write(&b_path, "fn b(){}").unwrap();
        let session = write_fixture(
            tmp.path(),
            &[a_path.to_str().unwrap(), b_path.to_str().unwrap()],
        );

        let mut keep = HashSet::new();
        keep.insert(fs::canonicalize(&a_path).unwrap());

        let mut stats_opt = None;
        with_clean_env(|| {
            stats_opt = Some(trim_session(&session, &keep).expect("trim ok"));
        });
        let stats = stats_opt.unwrap();
        assert!(!stats.skipped);
        assert!(stats.records_dropped >= 4, "should drop thinking + bash pair + b read pair, got {:?}", stats);

        let recs = read_records(&session);
        // Surviving tool_use ids: only tu-a; tu-b and tu-bash are gone.
        let tool_use_ids: Vec<String> = recs
            .iter()
            .filter_map(|r| {
                r.get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                    .and_then(|b| b.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            })
            .collect();
        assert_eq!(tool_use_ids, vec!["tu-a".to_string()]);

        // Every tool_use has a paired tool_result (validate() enforces this on write;
        // re-check here for the behavior we promise).
        let tool_result_ids: Vec<String> = recs
            .iter()
            .filter_map(|r| {
                r.get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
                    .and_then(|b| b.get("tool_use_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            })
            .collect();
        assert_eq!(tool_result_ids, vec!["tu-a".to_string()]);

        // No thinking blocks survive.
        for r in &recs {
            let typ_opt = r
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|b| b.get("type"))
                .and_then(|v| v.as_str());
            assert_ne!(typ_opt, Some("thinking"));
        }
    }

    #[test]
    fn trim_relinks_parent_uuids_to_nearest_surviving_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let a_path = tmp.path().join("a.rs");
        let b_path = tmp.path().join("b.rs");
        fs::write(&a_path, "x").unwrap();
        fs::write(&b_path, "y").unwrap();
        let session = write_fixture(
            tmp.path(),
            &[a_path.to_str().unwrap(), b_path.to_str().unwrap()],
        );

        // Keep only b. Records dropped between root and b-read should result
        // in u-read-b's new parent being u-root (the only surviving ancestor).
        let mut keep = HashSet::new();
        keep.insert(fs::canonicalize(&b_path).unwrap());

        with_clean_env(|| {
            trim_session(&session, &keep).expect("trim ok");
        });

        let recs = read_records(&session);
        let read_b = recs
            .iter()
            .find(|r| r.get("uuid").and_then(|v| v.as_str()) == Some("u-read-b"))
            .expect("u-read-b survives");
        let new_parent = read_b.get("parentUuid").and_then(|v| v.as_str());
        // u-think, u-read-a, u-res-a, u-bash, u-res-bash all dropped → parent
        // walks up to u-root.
        assert_eq!(new_parent, Some("u-root"));
    }

    #[test]
    fn trim_skipped_when_env_var_set() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_fixture(tmp.path(), &["/tmp/a.rs"]);
        let original = fs::read_to_string(&session).unwrap();

        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: serialized by ENV_LOCK above.
        unsafe { std::env::set_var("YOKE_DISABLE_SESSION_TRIM", "1") };
        let stats = trim_session(&session, &HashSet::new()).expect("trim ok");
        unsafe { std::env::remove_var("YOKE_DISABLE_SESSION_TRIM") };
        assert!(stats.skipped);
        assert_eq!(original, fs::read_to_string(&session).unwrap());
    }

    #[test]
    fn trim_empty_keep_drops_all_tool_use_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let session = write_fixture(tmp.path(), &["/tmp/a.rs", "/tmp/b.rs"]);

        with_clean_env(|| {
            trim_session(&session, &HashSet::new()).expect("trim ok");
        });

        let recs = read_records(&session);
        for r in &recs {
            let block = r
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|a| a.first());
            if let Some(b) = block {
                let btyp = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                assert_ne!(btyp, "tool_use", "no tool_use should survive empty keep");
                assert_ne!(btyp, "tool_result", "no tool_result should survive empty keep");
                assert_ne!(btyp, "thinking", "no thinking should survive");
            }
        }
        // queue-operation, initial user prompt should survive.
        assert!(recs.iter().any(|r| r.get("type").and_then(|v| v.as_str()) == Some("queue-operation")));
        assert!(recs.iter().any(|r| r.get("uuid").and_then(|v| v.as_str()) == Some("u-root")));
    }
}
