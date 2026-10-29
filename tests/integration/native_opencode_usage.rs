use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, get_binary_path};
use crate::test_utils::{fixture_path, isolated_metrics_db_path};
use git_ai::authorship::authorship_log_serialization::generate_session_id;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROOT: &str = "test-session-123";
const ASSISTANT: &str = "msg-assistant-sql-001";

struct NativeCase {
    repo: TestRepo,
    writer: Connection,
    native_path: PathBuf,
    database: String,
    base: i64,
    enabled: bool,
    capture_number: Cell<u64>,
    _storage: tempfile::TempDir,
    _metrics: tempfile::TempDir,
}

impl NativeCase {
    fn new(enabled: bool) -> Self {
        let storage = tempfile::tempdir().unwrap();
        let native_path = storage.path().join("opencode.db");
        fs::copy(fixture_path("opencode-sqlite/opencode.db"), &native_path).unwrap();
        let writer = Connection::open(&native_path).unwrap();
        writer.busy_timeout(Duration::from_secs(5)).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let base = (now - now % 86_400 - 86_400) * 1000;
        let old: i64 = writer
            .query_row(
                "SELECT time_created FROM session WHERE id=?1",
                [ROOT],
                |row| row.get(0),
            )
            .unwrap();
        let delta = base - old;
        writer.execute(
            "UPDATE session SET time_created=time_created+?1, time_updated=time_updated+?1, parent_id=NULL",
            [delta],
        ).unwrap();
        writer
            .execute(
                "UPDATE message SET time_created=time_created+?1, time_updated=time_updated+?1,
             data=CASE WHEN json_type(data, '$.time.completed')='integer'
               THEN json_set(data, '$.time.created', json_extract(data, '$.time.created')+?1,
                                   '$.time.completed', json_extract(data, '$.time.completed')+?1)
               ELSE json_set(data, '$.time.created', json_extract(data, '$.time.created')+?1) END",
                [delta],
            )
            .unwrap();
        writer.execute(
            "UPDATE part SET time_created=time_created+?1, time_updated=time_updated+?1,
             data=CASE WHEN json_type(data, '$.state.time.start')='integer'
               THEN json_set(data, '$.state.time.start', json_extract(data, '$.state.time.start')+?1,
                                   '$.state.time.end', json_extract(data, '$.state.time.end')+?1)
               ELSE data END",
            [delta],
        ).unwrap();
        let (metrics, database) = isolated_metrics_db_path();
        let mut repo = TestRepo::new_with_daemon_env(&[
            ("GIT_AI_TEST_METRICS_DB_PATH", &database),
            (
                "GIT_AI_OPENCODE_STORAGE_PATH",
                storage.path().to_str().unwrap(),
            ),
            (
                "GIT_AI_ENTERPRISE_METRICS",
                if enabled { "true" } else { "false" },
            ),
        ]);
        repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
        writer
            .execute(
                "UPDATE session SET directory=?1",
                [repo.canonical_path().to_str().unwrap()],
            )
            .unwrap();
        fs::write(repo.path().join("anchor.txt"), "human anchor\n").unwrap();
        repo.stage_all_and_commit("native usage anchor").unwrap();
        repo.filename("anchor.txt")
            .assert_committed_lines(crate::lines!["human anchor".unattributed_human(),]);
        let case = Self {
            repo,
            writer,
            native_path,
            database,
            base,
            enabled,
            capture_number: Cell::new(0),
            _storage: storage,
            _metrics: metrics,
        };
        case.replace(
            ASSISTANT,
            &assistant(base + 1000, "gpt-5", [100, 40, 10, 30, 20]),
        );
        case
    }

    fn session(&self, id: &str, parent: Option<&str>, created: i64) {
        self.writer.execute(
            "INSERT INTO session(id, project_id, parent_id, slug, directory, title, version, time_created, time_updated)
             SELECT ?1, project_id, ?2, slug || ?1, directory, title, version, ?3, ?3
             FROM session WHERE id=?4",
            params![id, parent, created, ROOT],
        ).unwrap();
    }

    fn message(&self, id: &str, session: &str, data: &Value) {
        let created = data["time"]["created"].as_i64().unwrap();
        self.writer.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data) VALUES(?1, ?2, ?3, ?3, ?4)",
            params![id, session, created, data.to_string()],
        ).unwrap();
    }

    fn replace(&self, id: &str, data: &Value) {
        assert_eq!(
            self.writer
                .execute(
                    "UPDATE message SET data=?1 WHERE id=?2",
                    params![data.to_string(), id]
                )
                .unwrap(),
            1
        );
    }

    fn capture(&self, session: &str) {
        let number = self.capture_number.get() + 1;
        self.capture_number.set(number);
        let file = self.repo.canonical_path().join("usage.rs");
        for event in ["PreToolUse", "PostToolUse"] {
            let input = json!({
                "hook_event_name": event, "session_id": session,
                "cwd": self.repo.canonical_path(), "tool_name": "edit",
                "tool_use_id": format!("native-usage-{number}"),
                "tool_input": { "filePath": file },
            })
            .to_string();
            self.repo
                .git_ai_with_env(
                    &["checkpoint", "opencode", "--hook-input", &input],
                    &[
                        (
                            "GIT_AI_OPENCODE_STORAGE_PATH",
                            self.native_path.parent().unwrap().to_str().unwrap(),
                        ),
                        ("GIT_AI_TEST_METRICS_DB_PATH", &self.database),
                        (
                            "GIT_AI_ENTERPRISE_METRICS",
                            if self.enabled { "true" } else { "false" },
                        ),
                    ],
                )
                .unwrap();
            if event == "PreToolUse" {
                fs::write(&file, format!("fn native_probe_{number}() {{}}\n")).unwrap();
            }
        }
    }

    fn drain(&self) {
        self.repo.sync_daemon();
        self.repo.git_ai(&["await", "--timeout", "60"]).unwrap();
    }

    fn restart(&mut self) {
        self.drain();
        self.repo.restart_dedicated_daemon_with_env_for_test(&[
            ("GIT_AI_TEST_METRICS_DB_PATH", &self.database),
            (
                "GIT_AI_OPENCODE_STORAGE_PATH",
                self.native_path.parent().unwrap().to_str().unwrap(),
            ),
            (
                "GIT_AI_ENTERPRISE_METRICS",
                if self.enabled { "true" } else { "false" },
            ),
        ]);
        self.drain();
    }

    fn export(&self) -> Value {
        let output = Command::new(get_binary_path())
            .args([
                "export-metrics",
                "--username",
                "employee-42",
                "--database",
                &self.database,
            ])
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn wait_export(&self, expected: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snapshot = self.export();
            if expected(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "unexpected native usage export: {snapshot}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn assert_outside_upload_history(&self) {
        let conn =
            Connection::open_with_flags(&self.database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let counts: (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(delivered_ts IS NULL), 0) FROM metrics WHERE event_kind=9 AND tool='opencode'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(
            counts,
            (0, 0),
            "new OpenCode TokenUsage must never enter ordinary metrics history or its pending queue"
        );
    }

    fn checkpoint_source_main(&self) -> Vec<u8> {
        let busy: i64 = self
            .writer
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy, 0);
        fs::read(&self.native_path).unwrap()
    }

    fn assert_wal_only(&self, main: &[u8]) {
        assert_eq!(fs::read(&self.native_path).unwrap(), main);
        assert!(
            fs::metadata(self.native_path.with_extension("db-wal"))
                .unwrap()
                .len()
                > 0
        );
    }
}

fn assistant(
    created: i64,
    model: &str,
    [input, output, reasoning, read, write]: [u64; 5],
) -> Value {
    json!({
        "role": "assistant", "time": {"created": created, "completed": created + 1000},
        "parentID": "user-message-is-not-session-ancestry", "modelID": model, "providerID": "openai",
        "tokens": {"input": input, "output": output, "reasoning": reasoning, "cache": {"read": read, "write": write}},
    })
}

fn bucket_ms(created: i64) -> u64 {
    (created - created % 300_000) as u64
}

fn rows<'a>(snapshot: &'a Value, native: &str) -> Vec<&'a Value> {
    let internal = generate_session_id(native, "opencode");
    snapshot["token_usage"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["agent_type"] == "opencode" && row["session_id"] == internal)
        .collect()
}

fn bucket<'a>(snapshot: &'a Value, native: &str, model: &str, time: u64) -> Option<&'a Value> {
    rows(snapshot, native)
        .into_iter()
        .find(|row| row["model"] == model && row["timestamp"] == time)
}

fn assert_counts(row: &Value, expected: [u64; 6]) {
    for (field, expected) in [
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "total_tokens",
    ]
    .into_iter()
    .zip(expected)
    {
        assert_eq!(row[field].as_u64(), Some(expected), "{field}: {row}");
    }
    assert_eq!(row["record_type"], "token_bucket");
    assert_eq!(row["bucket_duration_seconds"], 300);
}

fn unavailable(snapshot: &Value) -> bool {
    snapshot["coverage"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| {
            diagnostic["record_type"] == "token_bucket"
                && diagnostic["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.to_ascii_lowercase().contains("unavailable"))
        })
}

#[test]
fn native_opencode_cli_normalizes_usage_only_after_opt_in_and_never_reingests_it() {
    let mut case = NativeCase::new(false);
    case.capture(ROOT);
    case.drain();
    assert!(rows(&case.export(), ROOT).is_empty());
    case.assert_outside_upload_history();
    case.writer.execute(
        "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) VALUES('step-finish-copy', ?1, ?2, ?3, ?3, ?4)",
        params![ASSISTANT, ROOT, case.base + 2000, json!({
            "type": "step-finish", "tokens": {"input":100, "output":40, "reasoning":10, "cache":{"read":30, "write":20}},
        }).to_string()],
    ).unwrap();
    case.enabled = true;
    case.restart();
    case.capture(ROOT);
    case.drain();
    let snapshot = case.wait_export(|value| rows(value, ROOT).len() == 1);
    let row = rows(&snapshot, ROOT)[0];
    assert_counts(row, [100, 50, 10, 30, 20, 200]);
    assert_eq!(row["external_session_id"], ROOT);
    assert_eq!(row["timestamp"], bucket_ms(case.base + 1000));
    assert_eq!(row["root_relation_complete"], false);
    assert!(row["emitted_seq"].as_u64().unwrap() > 0);
    case.assert_outside_upload_history();
    case.repo.git_ai(&["reingest", "--all"]).unwrap();
    case.drain();
    case.assert_outside_upload_history();
    case.capture(ROOT);
    case.drain();
    let repeated = case.export();
    assert_eq!(snapshot["token_usage"], repeated["token_usage"]);
    assert_eq!(snapshot["token_root_totals"], repeated["token_root_totals"]);
    let serialized = repeated.to_string();
    assert!(!serialized.contains("Please update index.ts"));
    assert!(!serialized.contains("Edit applied"));
    assert!(!serialized.contains("native_probe_"));
}

#[test]
fn native_opencode_cli_aggregates_real_children_skips_fork_copies_and_retains_parent_evidence() {
    let mut case = NativeCase::new(true);
    case.session("native-child", Some(ROOT), case.base + 300_000);
    case.session("native-fork", Some(ROOT), case.base + 600_000);
    case.message(
        "child-new",
        "native-child",
        &assistant(case.base + 360_000, "gpt-5", [10, 3, 2, 4, 1]),
    );
    case.writer.execute(
        "INSERT INTO message(id, session_id, time_created, time_updated, data)
         SELECT 'fork-copy-with-new-id', 'native-fork', time_created, time_updated, data FROM message WHERE id=?1",
        [ASSISTANT],
    ).unwrap();
    case.message(
        "fork-new",
        "native-fork",
        &assistant(case.base + 660_000, "gpt-5", [20, 4, 1, 3, 2]),
    );
    for session in [ROOT, "native-child", "native-fork"] {
        case.capture(session);
    }
    case.drain();
    let snapshot = case.wait_export(|value| {
        [ROOT, "native-child", "native-fork"]
            .iter()
            .all(|native| rows(value, native).len() == 1)
    });
    assert_counts(rows(&snapshot, ROOT)[0], [100, 50, 10, 30, 20, 200]);
    assert_counts(rows(&snapshot, "native-child")[0], [10, 5, 2, 4, 1, 20]);
    assert_counts(rows(&snapshot, "native-fork")[0], [20, 5, 1, 3, 2, 30]);
    let internal_root = generate_session_id(ROOT, "opencode");
    for native in [ROOT, "native-child", "native-fork"] {
        let row = rows(&snapshot, native)[0];
        assert_eq!(row["external_session_id"], native);
        assert_eq!(row["root_session_id"], internal_root);
        assert_eq!(row["root_relation_complete"], false);
    }
    assert_eq!(
        rows(&snapshot, "native-child")[0]["parent_session_id"],
        internal_root
    );
    assert_eq!(
        rows(&snapshot, "native-fork")[0]["parent_session_id"],
        internal_root
    );
    assert_eq!(snapshot["token_root_totals"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["token_root_totals"][0]["total_tokens"], 250);
    assert_eq!(
        snapshot["token_root_totals"][0]["descendant_session_count"],
        2
    );
    assert_eq!(
        snapshot["token_root_totals"][0]["aggregation_complete"],
        false
    );
    let previous = rows(&snapshot, "native-child")[0]["emitted_seq"]
        .as_u64()
        .unwrap();
    case.writer
        .execute(
            "UPDATE session SET parent_id=NULL WHERE id='native-child'",
            [],
        )
        .unwrap();
    case.replace(
        "child-new",
        &assistant(case.base + 360_000, "gpt-5", [5, 3, 2, 4, 1]),
    );
    case.capture("native-child");
    case.drain();
    let corrected = case.wait_export(|value| {
        rows(value, "native-child")
            .first()
            .is_some_and(|row| row["total_tokens"] == 15)
    });
    assert_eq!(
        rows(&corrected, "native-child")[0]["root_session_id"],
        internal_root
    );
    assert!(
        rows(&corrected, "native-child")[0]["emitted_seq"]
            .as_u64()
            .unwrap()
            > previous
    );
    assert_eq!(corrected["token_root_totals"][0]["total_tokens"], 245);
    case.restart();
    for session in [ROOT, "native-child", "native-fork"] {
        case.capture(session);
    }
    case.drain();
    let repeated = case.export();
    assert_eq!(corrected["token_usage"], repeated["token_usage"]);
    assert_eq!(
        corrected["token_root_totals"],
        repeated["token_root_totals"]
    );
}

#[test]
fn native_opencode_cli_rescans_shared_wal_ties_decreases_moves_and_deletions() {
    let mut case = NativeCase::new(true);
    case.session("other-native", None, case.base);
    case.message(
        "other-message",
        "other-native",
        &assistant(case.base + 2000, "gpt-5", [7, 2, 1, 0, 0]),
    );
    for session in [ROOT, "other-native"] {
        case.capture(session);
    }
    case.drain();
    let initial = case.wait_export(|value| {
        rows(value, ROOT).len() == 1 && rows(value, "other-native").len() == 1
    });
    assert_counts(rows(&initial, "other-native")[0], [7, 3, 1, 0, 0, 10]);
    let initial_seq = rows(&initial, ROOT)[0]["emitted_seq"].as_u64().unwrap();
    let original_updated: i64 = case
        .writer
        .query_row(
            "SELECT time_updated FROM message WHERE id=?1",
            [ASSISTANT],
            |row| row.get(0),
        )
        .unwrap();
    let main = case.checkpoint_source_main();
    case.replace(
        ASSISTANT,
        &assistant(case.base + 1000, "gpt-5", [60, 15, 5, 10, 10]),
    );
    case.replace(
        "other-message",
        &assistant(case.base + 2000, "gpt-5", [9, 2, 1, 0, 0]),
    );
    case.assert_wal_only(&main);
    let tied_updated: i64 = case
        .writer
        .query_row(
            "SELECT time_updated FROM message WHERE id=?1",
            [ASSISTANT],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tied_updated, original_updated);
    case.restart();
    let lower = case.wait_export(|value| {
        rows(value, ROOT)
            .first()
            .is_some_and(|row| row["total_tokens"] == 100)
            && rows(value, "other-native")
                .first()
                .is_some_and(|row| row["total_tokens"] == 12)
    });
    assert_counts(rows(&lower, ROOT)[0], [60, 20, 5, 10, 10, 100]);
    assert!(rows(&lower, ROOT)[0]["emitted_seq"].as_u64().unwrap() > initial_seq);
    let old_bucket = bucket_ms(case.base + 1000);
    let moved_bucket = bucket_ms(case.base + 361_000);
    let main = case.checkpoint_source_main();
    case.replace(
        ASSISTANT,
        &assistant(case.base + 361_000, "gpt-5.1", [60, 15, 5, 10, 10]),
    );
    case.assert_wal_only(&main);
    case.restart();
    let moved = case.wait_export(|value| {
        bucket(value, ROOT, "gpt-5", old_bucket).is_some_and(|row| row["total_tokens"] == 0)
            && bucket(value, ROOT, "gpt-5.1", moved_bucket)
                .is_some_and(|row| row["total_tokens"] == 100)
    });
    assert_eq!(rows(&moved, ROOT).len(), 2);
    assert_counts(
        bucket(&moved, ROOT, "gpt-5.1", moved_bucket).unwrap(),
        [60, 20, 5, 10, 10, 100],
    );
    assert_counts(rows(&moved, "other-native")[0], [9, 3, 1, 0, 0, 12]);
    let main = case.checkpoint_source_main();
    case.writer
        .execute("DELETE FROM message WHERE id=?1", [ASSISTANT])
        .unwrap();
    case.assert_wal_only(&main);
    case.restart();
    let deleted = case.wait_export(|value| {
        rows(value, ROOT).len() == 2 && rows(value, ROOT).iter().all(|row| row["total_tokens"] == 0)
    });
    assert_counts(rows(&deleted, "other-native")[0], [9, 3, 1, 0, 0, 12]);
    assert!(!unavailable(&deleted));
    case.restart();
    for session in [ROOT, "other-native"] {
        case.capture(session);
    }
    case.drain();
    let repeated = case.export();
    assert_eq!(deleted["token_usage"], repeated["token_usage"]);
    assert_eq!(deleted["token_root_totals"], repeated["token_root_totals"]);
}

#[test]
fn native_opencode_cli_withholds_malformed_latest_snapshot_then_recovers_valid_usage() {
    let mut case = NativeCase::new(true);
    case.capture(ROOT);
    case.drain();
    let initial = case.wait_export(|value| rows(value, ROOT).len() == 1);
    assert_counts(rows(&initial, ROOT)[0], [100, 50, 10, 30, 20, 200]);
    let mut malformed = assistant(case.base + 60_000, "gpt-5", [10, 3, 2, 4, 1]);
    malformed["tokens"]["input"] = json!("private_invalid_counter_sentinel");
    case.message("latest-native-row", ROOT, &malformed);
    case.capture(ROOT);
    case.drain();
    let invalid = case.wait_export(|value| rows(value, ROOT).is_empty() && unavailable(value));
    assert!(invalid["token_root_totals"].as_array().unwrap().is_empty());
    assert!(
        !invalid
            .to_string()
            .contains("private_invalid_counter_sentinel")
    );
    case.restart();
    case.wait_export(|value| rows(value, ROOT).is_empty() && unavailable(value));
    case.replace(
        "latest-native-row",
        &assistant(case.base + 60_000, "gpt-5", [10, 3, 2, 4, 1]),
    );
    case.restart();
    case.capture(ROOT);
    case.drain();
    let recovered = case.wait_export(|value| {
        rows(value, ROOT)
            .first()
            .is_some_and(|row| row["total_tokens"] == 220)
            && !unavailable(value)
    });
    assert_eq!(rows(&recovered, ROOT).len(), 1);
    assert_counts(rows(&recovered, ROOT)[0], [110, 55, 12, 34, 21, 220]);
    assert!(
        rows(&recovered, ROOT)[0]["emitted_seq"].as_u64().unwrap()
            > rows(&initial, ROOT)[0]["emitted_seq"].as_u64().unwrap()
    );
    case.assert_outside_upload_history();
    case.capture(ROOT);
    case.drain();
    assert_eq!(recovered["token_usage"], case.export()["token_usage"]);
}

#[test]
fn native_opencode_cli_retains_first_invalid_child_parent_for_valid_grandchild() {
    let mut case = NativeCase::new(true);
    case.session("unavailable-child", Some(ROOT), case.base + 300_000);
    case.session(
        "valid-grandchild",
        Some("unavailable-child"),
        case.base + 600_000,
    );
    let mut malformed = assistant(case.base + 360_000, "gpt-5", [10, 3, 2, 4, 1]);
    malformed["tokens"]["input"] = json!("private_bad_first_child");
    case.message("bad-child-message", "unavailable-child", &malformed);
    case.message(
        "grandchild-message",
        "valid-grandchild",
        &assistant(case.base + 660_000, "gpt-5", [10, 3, 2, 4, 1]),
    );
    for session in [ROOT, "unavailable-child", "valid-grandchild"] {
        case.capture(session);
    }
    case.drain();
    let snapshot = case.wait_export(|value| {
        rows(value, ROOT).len() == 1
            && rows(value, "valid-grandchild").len() == 1
            && unavailable(value)
    });
    assert!(rows(&snapshot, "unavailable-child").is_empty());
    assert_eq!(
        rows(&snapshot, "valid-grandchild")[0]["root_session_id"],
        generate_session_id(ROOT, "opencode")
    );
    assert_eq!(snapshot["token_root_totals"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["token_root_totals"][0]["total_tokens"], 220);
    assert_eq!(
        snapshot["token_root_totals"][0]["aggregation_complete"],
        false
    );
    assert!(!snapshot.to_string().contains("private_bad_first_child"));
    case.restart();
    let repeated =
        case.wait_export(|value| rows(value, "valid-grandchild").len() == 1 && unavailable(value));
    assert_eq!(snapshot["token_root_totals"], repeated["token_root_totals"]);
    case.assert_outside_upload_history();
}

#[test]
fn token_parent_capture_is_opt_in_for_unchanged_claude_counters() {
    let mut case = NativeCase::new(false);
    let transcript = case
        .repo
        .canonical_path()
        .join("claude-parent/subagents/agent-local.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let timestamp = chrono::DateTime::from_timestamp(case.base / 1000 + 1000, 0)
        .unwrap()
        .to_rfc3339();
    fs::write(&transcript, format!("{}\n", json!({
        "timestamp": timestamp, "sessionId": "agent-local", "requestId": "claude-local-request",
        "message": { "id": "claude-local-message", "model": "claude-sonnet-4-20250514",
            "usage": { "input_tokens": 100, "output_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 } },
    }))).unwrap();
    let capture = |case: &NativeCase, contents: &str| {
        let file = case.repo.canonical_path().join("claude.rs");
        for event in ["PreToolUse", "PostToolUse"] {
            let input = json!({ "hook_event_name": event, "session_id": "agent-local", "cwd": case.repo.canonical_path(),
                "tool_name": "Write", "tool_use_id": "claude-local-edit", "transcript_path": transcript,
                "tool_input": { "file_path": file }, }).to_string();
            case.repo
                .git_ai(&["checkpoint", "claude", "--hook-input", &input])
                .unwrap();
            if event == "PreToolUse" {
                fs::write(&file, contents).unwrap();
            }
        }
        case.drain();
    };
    let counts = |case: &NativeCase| {
        let db =
            Connection::open_with_flags(&case.database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let ordinary: i64 = db.query_row("SELECT COUNT(*) FROM metrics WHERE event_kind=9 AND tool='claude' AND parent_session_id=?1",
            [generate_session_id("claude-parent", "claude")], |row| row.get(0)).unwrap();
        let private: i64 = db
            .query_row("SELECT COUNT(*) FROM enterprise_token_parents", [], |row| {
                row.get(0)
            })
            .unwrap();
        (ordinary, private)
    };
    capture(&case, "fn before_opt_in() {}\n");
    assert_eq!(
        counts(&case),
        (1, 0),
        "disabled enterprise capture must preserve ordinary parent metadata without new private observations"
    );
    let snapshot = case.export();
    assert_eq!(
        snapshot["token_usage"][0]["root_session_id"],
        generate_session_id("claude-parent", "claude")
    );
    case.enabled = true;
    case.restart();
    capture(&case, "fn after_opt_in() {}\n");
    assert_eq!(
        counts(&case),
        (1, 1),
        "unchanged counters must not prevent opt-in parent capture or emit another ordinary usage row"
    );
}
