//! Optional, opt-in reader that turns Claude/Codex session-file lifecycle
//! metadata into `AmbientInfo` counts for a pane's `AgentInfo`.
//!
//! Gated end-to-end by `AppState::ambient_reader_enabled`
//! (`[experimental] ambient_reader`, default off, restart-required to
//! change). When disabled, `compute_ambient` is never called and no session
//! file is ever opened.
//!
//! Privacy boundary: only lifecycle metadata is read into memory (task
//! identifiers, start/completion markers, status, exit code). Prompt text,
//! command strings, tool output, and file paths found in these records are
//! read only to the extent needed to detect the markers below and are never
//! stored past this function call, never logged, and never appear in
//! `AmbientInfo` - which has exactly three `u32` fields.
//!
//! Stateless, bounded: each call reads at most the trailing
//! [`MAX_TAIL_BYTES`] of the target file (matching the wire-contract note in
//! herdr-pet's docs/ambient-signals.md) and keeps no on-disk cursor. The only
//! in-memory state is `AmbientReaderCache`, an ephemeral session_id -> file
//! path map for Codex (whose rollout files live under a date-partitioned
//! directory with no direct id -> path mapping); it is rebuilt from
//! currently-live panes if the process restarts.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::api::schema::AmbientInfo;

const MAX_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// Ephemeral session_id -> resolved file path cache for Codex. See module
/// docs. Never persisted; safe to drop and rebuild at any time.
#[derive(Default)]
pub struct AmbientReaderCache {
    codex_paths: Mutex<HashMap<String, PathBuf>>,
}

/// Computes ambient counts for one pane, or `None` when the agent kind is
/// unsupported, the session id/cwd needed to locate the file is missing, or
/// the file cannot be found/read. Never panics on malformed input.
pub fn compute_ambient(
    cache: &AmbientReaderCache,
    agent: &str,
    session_id: &str,
    cwd: Option<&str>,
    home: &Path,
) -> Option<AmbientInfo> {
    match agent {
        "claude" => claude::compute(cwd?, session_id, home),
        "codex" => codex::compute(cache, session_id, home),
        _ => None,
    }
}

/// Reads at most the trailing `MAX_TAIL_BYTES` of `path`. When the read
/// starts mid-file, drops the first (possibly truncated) line so callers
/// never parse a partial JSON record.
fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        let after_first_line = text.find('\n').map(|idx| idx + 1).unwrap_or(text.len());
        return Some(text[after_first_line..].to_string());
    }
    Some(text)
}

mod claude {
    use super::*;
    use serde_json::Value;

    /// Claude Code stores a project's sessions under
    /// `~/.claude/projects/<cwd with '/' replaced by '-'>/<session_id>.jsonl`.
    pub(super) fn compute(cwd: &str, session_id: &str, home: &Path) -> Option<AmbientInfo> {
        if session_id.contains(['/', '\\']) || session_id.contains("..") {
            return None;
        }
        let slug = slugify_cwd(cwd);
        let path = home
            .join(".claude/projects")
            .join(slug)
            .join(format!("{session_id}.jsonl"));
        let tail = read_tail(&path)?;
        Some(parse_tail(&tail))
    }

    /// Claude Code's own project-directory naming: every `/`, `_`, and `.`
    /// in the cwd is flattened to `-` (observed against real
    /// `~/.claude/projects/*` directory names). This only matches a pane
    /// whose current cwd is the same directory the session was created in;
    /// a session resumed into a different cwd (or a project dir Claude
    /// itself renamed) is a known gap - it safely yields `None` via a failed
    /// file lookup rather than guessing a wrong directory.
    fn slugify_cwd(cwd: &str) -> String {
        cwd.chars()
            .map(|c| if matches!(c, '/' | '_' | '.') { '-' } else { c })
            .collect()
    }

    fn parse_tail(tail: &str) -> AmbientInfo {
        // task_use_id -> () for every Bash(run_in_background) / Task tool_use
        // seen. A later completion signal removes the entry.
        let mut background_started: HashMap<String, ()> = HashMap::new();
        let mut subagent_started: HashMap<String, ()> = HashMap::new();
        let mut failed_since_boundary: u32 = 0;

        for line in tail.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");

            if record_type == "user" || record_type == "assistant" {
                let raw_content = record.get("message").and_then(|m| m.get("content"));

                // A root-task boundary is a user turn whose content is a
                // plain string (a fresh prompt), not a tool_result list -
                // matches D-25: failures only reset on a new root task.
                if record_type == "user" && raw_content.is_some_and(Value::is_string) {
                    failed_since_boundary = 0;
                }

                let Some(content) = raw_content.and_then(Value::as_array) else {
                    continue;
                };

                for item in content {
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    if item_type == "tool_use" {
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let Some(id) = item.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        if name == "Bash"
                            && item
                                .get("input")
                                .and_then(|i| i.get("run_in_background"))
                                .and_then(Value::as_bool)
                                == Some(true)
                        {
                            background_started.insert(id.to_string(), ());
                        } else if name == "Task" {
                            subagent_started.insert(id.to_string(), ());
                        }
                    } else if item_type == "tool_result" {
                        let Some(id) = item.get("tool_use_id").and_then(Value::as_str) else {
                            continue;
                        };
                        // A synchronous tool_result on a background-Bash id is
                        // only the "queued" ack, not completion - Bash
                        // completion arrives via a queue-operation
                        // task-notification instead (handled below). A
                        // tool_result on a Task id, however, is the real
                        // subagent completion.
                        subagent_started.remove(id);
                    }
                }
            } else if record_type == "queue-operation" {
                let Some(content) = record.get("content").and_then(Value::as_str) else {
                    continue;
                };
                if !content.contains("<task-notification>") {
                    continue;
                }
                let Some(tool_use_id) = extract_tag(content, "tool-use-id") else {
                    continue;
                };
                let Some(status) = extract_tag(content, "status") else {
                    continue;
                };
                if background_started.remove(&tool_use_id).is_some()
                    && matches!(status.as_str(), "failed" | "killed")
                {
                    failed_since_boundary += 1;
                }
            }
        }

        AmbientInfo {
            subagents_active: subagent_started.len() as u32,
            background_running: background_started.len() as u32,
            background_failed: failed_since_boundary,
        }
    }

    /// Extracts the text inside `<tag>...</tag>` without pulling in an XML
    /// parser for one fixed, self-generated record shape. Only ever reads
    /// the tag content, never the surrounding `<summary>` text (which can
    /// carry a truncated command description) into `AmbientInfo`.
    fn extract_tag(haystack: &str, tag: &str) -> Option<String> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = haystack.find(&open)? + open.len();
        let end = haystack[start..].find(&close)? + start;
        Some(haystack[start..end].to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn bash_start(id: &str) -> String {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"run_in_background":true,"command":"sleep 999"}}}}]}}}}"#
            )
        }

        fn task_start(id: &str) -> String {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Task","input":{{"prompt":"do something"}}}}]}}}}"#
            )
        }

        fn task_result(id: &str) -> String {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"done"}}]}}}}"#
            )
        }

        fn notification(id: &str, status: &str) -> String {
            format!(
                r#"{{"type":"queue-operation","operation":"enqueue","content":"<task-notification>\n<tool-use-id>{id}</tool-use-id>\n<status>{status}</status>\n<summary>SENTINEL should never leak: rm -rf /</summary>\n</task-notification>"}}"#
            )
        }

        fn root_prompt(text: &str) -> String {
            format!(r#"{{"type":"user","message":{{"content":"{text}"}}}}"#)
        }

        #[test]
        fn background_task_with_no_completion_counts_as_running() {
            let tail = bash_start("t1");
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 1);
            assert_eq!(info.background_failed, 0);
        }

        #[test]
        fn completed_background_task_is_not_running_and_not_failed() {
            let tail = format!("{}\n{}", bash_start("t1"), notification("t1", "completed"));
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 0);
            assert_eq!(info.background_failed, 0);
        }

        #[test]
        fn failed_background_task_counts_as_failed_not_running() {
            let tail = format!("{}\n{}", bash_start("t1"), notification("t1", "failed"));
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 0);
            assert_eq!(info.background_failed, 1);
        }

        #[test]
        fn killed_background_task_counts_as_failed() {
            let tail = format!("{}\n{}", bash_start("t1"), notification("t1", "killed"));
            let info = parse_tail(&tail);
            assert_eq!(info.background_failed, 1);
        }

        #[test]
        fn new_root_task_resets_the_failed_count() {
            let tail = format!(
                "{}\n{}\n{}",
                bash_start("t1"),
                notification("t1", "failed"),
                root_prompt("next thing please")
            );
            let info = parse_tail(&tail);
            assert_eq!(
                info.background_failed, 0,
                "a new root task clears the prior failure count"
            );
        }

        #[test]
        fn task_tool_use_without_a_result_counts_as_an_active_subagent() {
            let tail = task_start("s1");
            let info = parse_tail(&tail);
            assert_eq!(info.subagents_active, 1);
        }

        #[test]
        fn task_tool_use_with_a_result_is_no_longer_active() {
            let tail = format!("{}\n{}", task_start("s1"), task_result("s1"));
            let info = parse_tail(&tail);
            assert_eq!(info.subagents_active, 0);
        }

        #[test]
        fn malformed_json_lines_are_skipped_without_panicking() {
            let tail = format!("not json at all\n{}\n{{broken", bash_start("t1"));
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 1);
        }

        #[test]
        fn sentinel_summary_text_never_reaches_ambient_info() {
            let tail = format!("{}\n{}", bash_start("t1"), notification("t1", "failed"));
            let info = parse_tail(&tail);
            let serialized = serde_json::to_string(&info).unwrap();
            assert!(!serialized.contains("SENTINEL"));
            assert!(!serialized.contains("rm -rf"));
        }
    }
}

mod codex {
    use super::*;
    use serde_json::Value;

    /// Codex rollout files live under
    /// `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<session_id>.jsonl`
    /// with no direct id -> path mapping, so this walks the date-partitioned
    /// tree once per unresolved session id and caches the match. The walk
    /// only ever matches against the exact session id already known from
    /// this pane's `agent_session` (never a broad, unscoped scan).
    pub(super) fn compute(
        cache: &AmbientReaderCache,
        session_id: &str,
        home: &Path,
    ) -> Option<AmbientInfo> {
        if session_id.contains(['/', '\\']) || session_id.contains("..") {
            return None;
        }
        let path = resolve_path(cache, session_id, home)?;
        let tail = read_tail(&path)?;
        Some(parse_tail(&tail))
    }

    fn resolve_path(cache: &AmbientReaderCache, session_id: &str, home: &Path) -> Option<PathBuf> {
        if let Ok(cached) = cache.codex_paths.lock() {
            if let Some(path) = cached.get(session_id) {
                if path.exists() {
                    return Some(path.clone());
                }
            }
        }
        let sessions_dir = home.join(".codex/sessions");
        let found = find_rollout_file(&sessions_dir, session_id)?;
        if let Ok(mut cached) = cache.codex_paths.lock() {
            cached.insert(session_id.to_string(), found.clone());
        }
        Some(found)
    }

    fn find_rollout_file(root: &Path, session_id: &str) -> Option<PathBuf> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".jsonl") && name.contains(session_id) {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Approximate outstanding-exec counter, per the documented uncertainty
    /// that a `custom_tool_call_output` "cell ID" and a later
    /// `item_completed` CommandExecution's `id`/`process_id` cannot be
    /// reliably correlated (see /tmp/fable-codex-background-lifecycle.md,
    /// section 6). This counts starts and completions in file order rather
    /// than matching identifiers - an approximation, not exact tracking.
    /// Codex subagent (`spawn_agent`) lifecycle has no confirmed completion
    /// signal at all, so `subagents_active` is always 0 for Codex; this is a
    /// documented limitation, not a bug (never fabricate an unconfirmed
    /// signal - see docs/ambient-signals.md).
    fn parse_tail(tail: &str) -> AmbientInfo {
        let mut running: i64 = 0;
        let mut failed_since_boundary: u32 = 0;

        for line in tail.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let payload = record.get("payload");
            let payload_type = payload
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");

            match payload_type {
                "task_started" => failed_since_boundary = 0,
                "custom_tool_call_output" => {
                    let output = payload
                        .and_then(|p| p.get("output"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if output.contains("Script running with cell ID") {
                        running += 1;
                    }
                }
                "item_completed" => {
                    let item = payload.and_then(|p| p.get("item"));
                    let item_type = item.and_then(|i| i.get("type")).and_then(Value::as_str);
                    if item_type == Some("CommandExecution") {
                        running = (running - 1).max(0);
                        let status = item.and_then(|i| i.get("status")).and_then(Value::as_str);
                        if status == Some("failed") {
                            failed_since_boundary += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        AmbientInfo {
            subagents_active: 0,
            background_running: running.max(0) as u32,
            background_failed: failed_since_boundary,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn running_signal() -> String {
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":"Script running with cell ID 29\nWall time 10.0 seconds\nOutput:\n"}}"#.to_string()
        }

        fn completed_signal(status: &str) -> String {
            format!(
                r#"{{"type":"response_item","payload":{{"type":"item_completed","item":{{"type":"CommandExecution","id":"exec-1","status":"{status}","exit_code":0,"command":"SENTINEL rm -rf /"}}}}}}"#
            )
        }

        fn task_started() -> String {
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#.to_string()
        }

        #[test]
        fn running_signal_with_no_completion_counts_as_running() {
            let info = parse_tail(&running_signal());
            assert_eq!(info.background_running, 1);
        }

        #[test]
        fn completed_signal_clears_the_running_counter() {
            let tail = format!("{}\n{}", running_signal(), completed_signal("completed"));
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 0);
            assert_eq!(info.background_failed, 0);
        }

        #[test]
        fn failed_signal_increments_failed_and_clears_running() {
            let tail = format!("{}\n{}", running_signal(), completed_signal("failed"));
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 0);
            assert_eq!(info.background_failed, 1);
        }

        #[test]
        fn task_started_resets_failed_count() {
            let tail = format!(
                "{}\n{}\n{}",
                running_signal(),
                completed_signal("failed"),
                task_started()
            );
            let info = parse_tail(&tail);
            assert_eq!(info.background_failed, 0);
        }

        #[test]
        fn subagents_active_is_always_zero_for_codex() {
            let info = parse_tail("");
            assert_eq!(info.subagents_active, 0);
        }

        #[test]
        fn malformed_lines_are_skipped_without_panicking() {
            let tail = format!("not json\n{}\n{{broken", running_signal());
            let info = parse_tail(&tail);
            assert_eq!(info.background_running, 1);
        }

        #[test]
        fn sentinel_command_text_never_reaches_ambient_info() {
            let tail = format!("{}\n{}", running_signal(), completed_signal("failed"));
            let info = parse_tail(&tail);
            let serialized = serde_json::to_string(&info).unwrap();
            assert!(!serialized.contains("SENTINEL"));
            assert!(!serialized.contains("rm -rf"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_agent_kind_returns_none() {
        let cache = AmbientReaderCache::default();
        assert_eq!(
            compute_ambient(
                &cache,
                "pi",
                "abc",
                Some("/tmp"),
                Path::new("/tmp/does-not-exist")
            ),
            None
        );
    }

    #[test]
    fn missing_cwd_returns_none_for_claude() {
        let cache = AmbientReaderCache::default();
        assert_eq!(
            compute_ambient(&cache, "claude", "abc", None, Path::new("/tmp")),
            None
        );
    }

    #[test]
    fn nonexistent_session_file_returns_none_without_panicking() {
        let cache = AmbientReaderCache::default();
        let home = std::env::temp_dir().join(format!("herdr-ambient-test-{}", std::process::id()));
        assert_eq!(
            compute_ambient(&cache, "claude", "does-not-exist", Some("/tmp/proj"), &home),
            None
        );
        assert_eq!(
            compute_ambient(&cache, "codex", "does-not-exist", None, &home),
            None
        );
    }

    #[test]
    fn session_id_path_traversal_is_rejected() {
        let cache = AmbientReaderCache::default();
        assert_eq!(
            compute_ambient(
                &cache,
                "claude",
                "../../etc/passwd",
                Some("/tmp/proj"),
                Path::new("/tmp")
            ),
            None
        );
        assert_eq!(
            compute_ambient(&cache, "codex", "../../etc/passwd", None, Path::new("/tmp")),
            None
        );
    }

    #[test]
    fn end_to_end_reads_a_real_claude_session_file_on_disk() {
        let dir = std::env::temp_dir().join(format!("herdr-ambient-e2e-{}", std::process::id()));
        let project_dir = dir.join(".claude/projects/-tmp-demo");
        std::fs::create_dir_all(&project_dir).unwrap();
        let session_path = project_dir.join("sess-1.jsonl");
        std::fs::write(
            &session_path,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"run_in_background":true}}]}}"#,
                "\n",
                r#"{"type":"queue-operation","operation":"enqueue","content":"<task-notification>\n<tool-use-id>t1</tool-use-id>\n<status>failed</status>\n</task-notification>"}"#,
                "\n",
            ),
        )
        .unwrap();

        let cache = AmbientReaderCache::default();
        let info = compute_ambient(&cache, "claude", "sess-1", Some("/tmp/demo"), &dir).unwrap();
        assert_eq!(info.background_running, 0);
        assert_eq!(info.background_failed, 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
