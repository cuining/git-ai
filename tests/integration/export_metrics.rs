use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, get_binary_path};
use crate::test_utils::{codex_checkpoint, isolated_metrics_db_path};
use git_ai::metrics::PosEncoded;
use git_ai::metrics::attrs::EventAttributes;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::events::{SessionEventValues, TokenUsageValues};
use git_ai::metrics::types::MetricEvent;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

fn export(database: &str, extra: &[&str]) -> std::process::Output {
    Command::new(get_binary_path())
        .args([
            "export-metrics",
            "--username",
            "employee-42",
            "--database",
            database,
        ])
        .args(extra)
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .output()
        .expect("export process should run")
}

fn exported(database: &str, extra: &[&str]) -> Value {
    let output = export(database, extra);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("export must contain only JSON")
}

fn ai_write(repo: &TestRepo, path: &str, contents: &str) {
    let path = repo.path().join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    codex_checkpoint(repo, &path, "export-session", "PreToolUse", "export-edit");
    fs::write(&path, contents).unwrap();
    codex_checkpoint(repo, &path, "export-session", "PostToolUse", "export-edit");
}

fn enterprise_repo(database: &str) -> TestRepo {
    TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_METRICS_DB_PATH", database),
        ("GIT_AI_ENTERPRISE_METRICS", "true"),
    ])
}

fn exported_commit(database: &str, sha: &str) -> Value {
    let snapshot = exported(database, &[]);
    let records: Vec<_> = snapshot["commits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["commit"] == sha)
        .collect();
    assert_eq!(
        records.len(),
        1,
        "commit {sha} must be exported exactly once"
    );
    records[0].clone()
}

#[test]
fn export_metrics_enterprise_source_is_independent_of_final_acceptance() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    ai_write(&repo, "src/main.rs", "let answer = 42;\nlet other = 7;\n");
    fs::write(
        repo.path().join("src/main.rs"),
        "let answer = 43;\nlet other = 7;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "src/main.rs"])
        .unwrap();
    let commit = repo.stage_all_and_commit("mixed source").unwrap();
    repo.filename("src/main.rs")
        .assert_committed_lines(crate::lines![
            "let answer = 43;".human(),
            "let other = 7;".ai(),
        ]);
    let record = exported_commit(&database, &commit.commit_sha);
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(
        record["ai_breakdown"][0]["prod_ai_surviving_source_line_num"],
        2
    );
    assert_eq!(record["ai_breakdown"][0]["agent_type"], "codex");
    assert!(record["prod_ai_code_line_gen_num"].is_null());
    assert_eq!(
        record["source_evidence"],
        "partial_verified_surviving_bytes"
    );
    assert_eq!(record["diff_coverage"], "complete_first_parent");
}

#[test]
fn export_metrics_enterprise_source_survives_partial_staging_initial_and_crlf() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    ai_write(
        &repo,
        "partial.rs",
        "let staged = 11;\r\nlet carried = 21;\r\nlet removed = 31;\r\n",
    );
    let final_text = "let staged = 12;\r\nlet carried = 22;\r\n";
    fs::write(repo.path().join("partial.rs"), final_text).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "partial.rs"])
        .unwrap();
    fs::write(repo.path().join("partial.rs"), "let staged = 12;\r\n").unwrap();
    repo.git(&["add", "partial.rs"]).unwrap();
    fs::write(repo.path().join("partial.rs"), final_text).unwrap();
    let first = repo.commit("stage one source line").unwrap();
    repo.filename("partial.rs")
        .assert_committed_lines(crate::lines!["let staged = 12;".human()]);
    let first_record = exported_commit(&database, &first.commit_sha);
    assert_eq!(first_record["prod_code_line_num"], 1);
    assert_eq!(first_record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(first_record["prod_ai_surviving_source_line_num"], 1);
    fs::write(
        repo.path().join("partial.rs"),
        final_text.replace("\r\n", "\n"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "human", "partial.rs"]).unwrap();
    let second = repo
        .stage_all_and_commit("carry source only identity")
        .unwrap();
    repo.filename("partial.rs")
        .assert_committed_lines(crate::lines![
            "let staged = 12;".human(),
            "let carried = 22;".human(),
        ]);
    let second_record = exported_commit(&database, &second.commit_sha);
    assert_eq!(second_record["prod_code_line_num"], 2);
    assert_eq!(second_record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(second_record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(second_record["ai_breakdown"][0]["agent_type"], "codex");
    assert!(second_record["prod_ai_code_line_gen_num"].is_null());
}

#[test]
fn export_metrics_enterprise_denominator_includes_recognition_ignored_files() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    ai_write(&repo, "src/main.rs", "fn main() {}\n");
    let files = [
        ("tests/a.rs", "test line\n"),
        ("README.md", "heading\nbody\n"),
        ("notes.txt", "text line\n"),
        ("中文.rs", "unicode line\n"),
        ("dir b/file.rs", "embedded path marker\n"),
        ("vendor/a.rs", "vendor line\n"),
        ("node_modules/a.js", "dependency line\n"),
        ("Cargo.lock", "other lock\n"),
        ("nested/.gitmodules", "nested modules\n"),
        ("page.lock", "excluded root lock\n"),
        ("nested/page.lock", "excluded nested lock\n"),
        (".gitmodules", "excluded root modules\n"),
    ];
    for (path, contents) in files {
        let target = repo.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents).unwrap();
    }
    fs::write(repo.path().join("binary.dat"), [0, 1, 2, 3]).unwrap();
    let commit = repo
        .stage_all_and_commit("full initial denominator")
        .unwrap();
    repo.filename("src/main.rs")
        .assert_committed_lines(crate::lines!["fn main() {}".ai()]);
    for (path, contents) in files {
        repo.filename(path).assert_committed_lines(
            contents
                .lines()
                .map(|line| line.unattributed_human())
                .collect(),
        );
    }
    let record = exported_commit(&database, &commit.commit_sha);
    assert_eq!(record["prod_code_line_num"], 10);
    assert_eq!(record["test_code_line_num"], 1);
    assert_eq!(record["changed_file_num"], 11);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(record["diff_coverage"], "complete_first_parent");
    let config = _dir.path().join("classification.json");
    fs::write(
        &config,
        r#"{"test_patterns":["vendor/**"],"language_extensions":{"rs":"InternalRust"}}"#,
    )
    .unwrap();
    let custom = exported(&database, &["--config", config.to_str().unwrap()]);
    let record = custom["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["commit"] == commit.commit_sha)
        .unwrap();
    assert_eq!(record["test_code_line_num"], 1);
    let languages: Value =
        serde_json::from_str(record["language_stats"].as_str().unwrap()).unwrap();
    assert_eq!(languages["InternalRust"]["test_code_line_num"], 1);
}

#[test]
fn export_metrics_enterprise_partial_source_maps_shifted_lines_and_renames() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    ai_write(
        &repo,
        "original 中.rs",
        "let unstaged = 11;\nlet staged_with_unique_retained_context_and_enough_bytes = 21;\n",
    );
    let final_text =
        "let unstaged = 12;\nlet staged_with_unique_retained_context_and_enough_bytes = 22;\n";
    fs::write(repo.path().join("original 中.rs"), final_text).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "original 中.rs"])
        .unwrap();
    fs::write(
        repo.path().join("original 中.rs"),
        "let staged_with_unique_retained_context_and_enough_bytes = 22;\n",
    )
    .unwrap();
    repo.git(&["add", "original 中.rs"]).unwrap();
    fs::write(repo.path().join("original 中.rs"), final_text).unwrap();
    let first = repo.commit("shifted partial source").unwrap();
    repo.filename("original 中.rs")
        .assert_committed_lines(crate::lines![
            "let staged_with_unique_retained_context_and_enough_bytes = 22;".human()
        ]);
    let record = exported_commit(&database, &first.commit_sha);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    repo.git(&["mv", "original 中.rs", "renamed 中.rs"])
        .unwrap();
    fs::write(
        repo.path().join("renamed 中.rs"),
        final_text.replace("unstaged = 12", "unstaged = 13"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "renamed 中.rs"])
        .unwrap();
    let second = repo
        .stage_all_and_commit("rename with carried source")
        .unwrap();
    repo.filename("renamed 中.rs")
        .assert_committed_lines(crate::lines![
            "let unstaged = 13;".human(),
            "let staged_with_unique_retained_context_and_enough_bytes = 22;".human(),
        ]);
    let record = exported_commit(&database, &second.commit_sha);
    assert_eq!(record["prod_code_line_num"], 1);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["changed_file_num"], 1);
}

#[test]
fn export_metrics_enterprise_structural_file_policy_and_gitlinks() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    fs::write(repo.path().join("anchor.txt"), "anchor\n").unwrap();
    let base = repo.stage_all_and_commit("anchor").unwrap();
    repo.filename("anchor.txt")
        .assert_committed_lines(crate::lines!["anchor".unattributed_human()]);
    for (path, contents) in [
        ("old.txt", "rename me\n"),
        ("mode.sh", "exit 0\n"),
        ("delete.md", "heading\nbody\n"),
    ] {
        fs::write(repo.path().join(path), contents).unwrap();
    }
    fs::write(repo.path().join("binary.dat"), [0, 1, 2]).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},submodule", base.commit_sha),
    ])
    .unwrap();
    let first = repo.commit("structural baseline").unwrap();
    for (path, contents) in [
        ("anchor.txt", "anchor"),
        ("old.txt", "rename me"),
        ("mode.sh", "exit 0"),
    ] {
        repo.filename(path)
            .assert_committed_lines(crate::lines![contents.unattributed_human()]);
    }
    repo.filename("delete.md")
        .assert_committed_lines(crate::lines![
            "heading".unattributed_human(),
            "body".unattributed_human()
        ]);
    let record = exported_commit(&database, &first.commit_sha);
    assert_eq!(record["prod_code_line_num"], 4);
    assert_eq!(record["changed_file_num"], 4);
    repo.git(&["mv", "old.txt", "renamed.txt"]).unwrap();
    repo.git(&["update-index", "--chmod=+x", "mode.sh"])
        .unwrap();
    repo.git(&["rm", "delete.md"]).unwrap();
    fs::write(repo.path().join("binary.dat"), [0, 9, 8, 7]).unwrap();
    repo.git(&["add", "binary.dat"]).unwrap();
    repo.git(&[
        "update-index",
        "--cacheinfo",
        &format!("160000,{},submodule", first.commit_sha),
    ])
    .unwrap();
    let second = repo
        .commit("binary rename mode deletion and gitlink")
        .unwrap();
    for (path, contents) in [
        ("anchor.txt", "anchor"),
        ("renamed.txt", "rename me"),
        ("mode.sh", "exit 0"),
    ] {
        repo.filename(path)
            .assert_committed_lines(crate::lines![contents.unattributed_human()]);
    }
    let record = exported_commit(&database, &second.commit_sha);
    assert_eq!(record["prod_code_line_num"], 0);
    assert_eq!(record["prod_code_delete_line_num"], 2);
    assert_eq!(record["changed_file_num"], 4);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["source_evidence"], "unavailable");
    repo.git(&["rm", "--cached", "mode.sh"]).unwrap();
    fs::remove_file(repo.path().join("mode.sh")).unwrap();
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},mode.sh", first.commit_sha),
    ])
    .unwrap();
    fs::write(repo.path().join("submodule"), "regular module content\n").unwrap();
    repo.git(&["add", "submodule"]).unwrap();
    let converted = repo.commit("regular and gitlink type changes").unwrap();
    for (path, contents) in [
        ("anchor.txt", "anchor"),
        ("renamed.txt", "rename me"),
        ("submodule", "regular module content"),
    ] {
        repo.filename(path)
            .assert_committed_lines(crate::lines![contents.unattributed_human()]);
    }
    let record = exported_commit(&database, &converted.commit_sha);
    assert_eq!(record["changed_file_num"], 2);
    assert_eq!(record["prod_code_line_num"], 1);
    assert_eq!(record["prod_code_delete_line_num"], 1);
}

#[test]
fn export_metrics_enterprise_capture_is_disabled_by_default_and_local_only() {
    assert!(!git_ai::feature_flags::FeatureFlags::default().enterprise_metrics);
    let (_dir, database) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", &database)]);
    ai_write(&repo, "disabled.rs", "let disabled = 1;\n");
    let commit = repo.stage_all_and_commit("disabled collection").unwrap();
    repo.filename("disabled.rs")
        .assert_committed_lines(crate::lines!["let disabled = 1;".ai()]);
    let record = exported_commit(&database, &commit.commit_sha);
    assert_eq!(record["diff_coverage"], "recorded_text_hunks");
    assert_eq!(record["source_evidence"], "unavailable");
    assert!(record["prod_ai_surviving_source_line_num"].is_null());
    let conn = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM enterprise_commit_facts", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    let (_enabled_dir, enabled_database) = isolated_metrics_db_path();
    let enabled = enterprise_repo(&enabled_database);
    ai_write(&enabled, "enabled.rs", "let enabled = 1;\n");
    let commit = enabled
        .stage_all_and_commit("enabled local collection")
        .unwrap();
    enabled
        .filename("enabled.rs")
        .assert_committed_lines(crate::lines!["let enabled = 1;".ai()]);
    let record = exported_commit(&enabled_database, &commit.commit_sha);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    let conn = rusqlite::Connection::open(enabled_database).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM enterprise_commit_facts", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    assert_eq!(conn.query_row("SELECT count(*) FROM metrics WHERE event_json LIKE '%ai_source%' OR event_json LIKE '%source_snapshot%' OR event_json LIKE '%legacy_event_hashes%'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn export_metrics_enterprise_reads_legacy_schema_without_mutation_and_redacts_urls() {
    let (_dir, database) = isolated_metrics_db_path();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch("CREATE TABLE metrics (id INTEGER PRIMARY KEY, event_json TEXT NOT NULL); CREATE TABLE schema_metadata (key TEXT PRIMARY KEY, value TEXT); INSERT INTO schema_metadata VALUES ('version', '2');").unwrap();
    for (id, repo_url) in ["https://credential_user:credential_password@example.com/owner/repo.git?credential_query#credential_fragment", "git@example.com:owner/repo.git?scp_query#scp_fragment"].into_iter().enumerate() {
        let mut event = token(&format!("session-{id}"), None, 1, 1, 1_800_000_001);
        event.attrs.insert(git_ai::metrics::attrs::attr_pos::REPO_URL.to_string(), json!(repo_url));
        conn.execute("INSERT INTO metrics(event_json) VALUES (?1)", [serde_json::to_string(&event).unwrap()]).unwrap();
    }
    drop(conn);
    let before = fs::read(&database).unwrap();
    let snapshot = exported(&database, &[]);
    assert_eq!(snapshot["token_usage"].as_array().unwrap().len(), 2);
    for record in snapshot["token_usage"].as_array().unwrap() {
        assert_eq!(record["repo"], "https://example.com/owner/repo");
    }
    assert_eq!(before, fs::read(&database).unwrap());
    let config = _dir.path().join("private-invalid.json");
    fs::write(&config, r#"{"test_patterns":"private_config_scalar"}"#).unwrap();
    let output = export(&database, &["--config", config.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private_config_scalar"));
}

#[test]
fn export_metrics_enterprise_retains_source_input_after_persistence_failure() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    let _db = MetricsDatabase::open_at_path(Path::new(&database)).unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_enterprise_facts BEFORE INSERT ON enterprise_commit_facts BEGIN SELECT RAISE(FAIL, 'test persistence failure'); END;").unwrap();
    ai_write(&repo, "retained.rs", "let retained = 1;\n");
    let commit = repo
        .stage_all_and_commit("successful git commit with unavailable capture")
        .unwrap();
    repo.filename("retained.rs")
        .assert_committed_lines(crate::lines!["let retained = 1;".ai()]);
    assert_eq!(
        exported_commit(&database, &commit.commit_sha)["diff_coverage"],
        "recorded_text_hunks"
    );
    let checkpoints = repo
        .path()
        .join(".git/ai/working_logs/initial/checkpoints.jsonl");
    let checkpoints = fs::read_to_string(checkpoints)
        .expect("failed capture must retain the working log for retry");
    assert!(checkpoints.contains("ai_source"));
}

#[test]
fn export_metrics_enterprise_discards_source_only_initial_after_checkout() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    fs::write(repo.path().join("discarded.rs"), "let base = 0;\n").unwrap();
    fs::write(repo.path().join("anchor.txt"), "anchor\n").unwrap();
    repo.stage_all_and_commit("baseline").unwrap();
    repo.filename("discarded.rs")
        .assert_committed_lines(crate::lines!["let base = 0;".unattributed_human()]);
    repo.filename("anchor.txt")
        .assert_committed_lines(crate::lines!["anchor".unattributed_human()]);
    ai_write(
        &repo,
        "discarded.rs",
        "let base = 0;\nlet alpha = 11;\nlet beta = 21;\n",
    );
    let retyped = "let base = 0;\nlet alpha = 12;\nlet beta = 22;\n";
    fs::write(repo.path().join("discarded.rs"), retyped).unwrap();
    repo.git_ai(&["checkpoint", "human", "discarded.rs"])
        .unwrap();
    fs::write(repo.path().join("anchor.txt"), "changed anchor\n").unwrap();
    repo.git(&["add", "anchor.txt"]).unwrap();
    let partial = repo
        .commit("carry source without final AI ownership")
        .unwrap();
    repo.filename("discarded.rs")
        .assert_committed_lines(crate::lines!["let base = 0;".unattributed_human()]);
    repo.filename("anchor.txt")
        .assert_committed_lines(crate::lines!["changed anchor".unattributed_human()]);
    let initial: Value = serde_json::from_slice(
        &fs::read(
            repo.path()
                .join(".git/ai/working_logs")
                .join(&partial.commit_sha)
                .join("INITIAL"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(initial["files"].as_object().unwrap().is_empty());
    assert!(initial["ai_sources"]["discarded.rs"].is_object());
    repo.git(&["checkout", "--", "discarded.rs"]).unwrap();
    fs::write(repo.path().join("discarded.rs"), retyped).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "discarded.rs"])
        .unwrap();
    let final_commit = repo
        .stage_all_and_commit("human retypes previously discarded text")
        .unwrap();
    repo.filename("anchor.txt")
        .assert_committed_lines(crate::lines!["changed anchor".unattributed_human()]);
    repo.filename("discarded.rs")
        .assert_committed_lines(crate::lines![
            "let base = 0;".unattributed_human(),
            "let alpha = 12;".human(),
            "let beta = 22;".human()
        ]);
    let record = exported_commit(&database, &final_commit.commit_sha);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
}

#[test]
fn export_metrics_enterprise_diff_context_counts_only_changed_lines() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    fs::write(
        repo.path().join("context.txt"),
        "first original\nmiddle unchanged\nlast original\n",
    )
    .unwrap();
    repo.stage_all_and_commit("baseline").unwrap();
    repo.filename("context.txt")
        .assert_committed_lines(crate::lines![
            "first original".unattributed_human(),
            "middle unchanged".unattributed_human(),
            "last original".unattributed_human()
        ]);
    repo.git(&["config", "diff.interHunkContext", "10"])
        .unwrap();
    fs::write(
        repo.path().join("context.txt"),
        "first edited\nmiddle unchanged\nlast edited\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "context.txt"])
        .unwrap();
    let commit = repo
        .stage_all_and_commit("two changes separated by unchanged text")
        .unwrap();
    repo.filename("context.txt")
        .assert_committed_lines(crate::lines![
            "first edited".human(),
            "middle unchanged".unattributed_human(),
            "last edited".human()
        ]);
    let record = exported_commit(&database, &commit.commit_sha);
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_code_delete_line_num"], 2);
}

#[test]
fn export_metrics_enterprise_rename_does_not_restore_erased_source() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = enterprise_repo(&database);
    let retained =
        "retained context one with enough bytes\nretained context two with enough bytes\n";
    fs::write(repo.path().join("negative 中.txt"), retained).unwrap();
    repo.stage_all_and_commit("tracked retained context")
        .unwrap();
    repo.filename("negative 中.txt")
        .assert_committed_lines(crate::lines![
            "retained context one with enough bytes".unattributed_human(),
            "retained context two with enough bytes".unattributed_human()
        ]);
    ai_write(&repo, "negative 中.txt", &format!("{retained}AAA\n"));
    repo.git(&["mv", "negative 中.txt", "negative renamed 中.txt"])
        .unwrap();
    for text in ["BBB", "AAA"] {
        fs::write(
            repo.path().join("negative renamed 中.txt"),
            format!("{retained}{text}\n"),
        )
        .unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", "negative renamed 中.txt"])
            .unwrap();
    }
    let commit = repo
        .stage_all_and_commit("human erases and retypes after rename")
        .unwrap();
    repo.filename("negative renamed 中.txt")
        .assert_committed_lines(crate::lines![
            "retained context one with enough bytes".unattributed_human(),
            "retained context two with enough bytes".unattributed_human(),
            "AAA".human()
        ]);
    let record = exported_commit(&database, &commit.commit_sha);
    assert_eq!(record["prod_code_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
}

#[test]
fn export_metrics_classifies_real_commits_and_excludes_source_content() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", &database)]);
    ai_write(&repo, "src/main.rs", "fn private_source_sentinel() {}\n");
    ai_write(&repo, "tests/main.rs", "#[test]\nfn example() {}\n");
    fs::write(repo.path().join("page.lock"), "excluded lock\n").unwrap();
    let commit = repo
        .stage_all_and_commit("private_commit_sentinel")
        .unwrap();
    repo.filename("src/main.rs")
        .assert_committed_lines(crate::lines!["fn private_source_sentinel() {}".ai()]);
    repo.filename("tests/main.rs")
        .assert_committed_lines(crate::lines!["#[test]".ai(), "fn example() {}".ai()]);
    repo.filename("page.lock")
        .assert_committed_lines(crate::lines!["excluded lock".unattributed_human()]);

    let snapshot = exported(&database, &[]);
    assert_eq!(snapshot["schema_version"], "git-ai.enterprise-metrics/1");
    let record = snapshot["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["commit"] == commit.commit_sha)
        .expect("real commit must be exported");
    assert_eq!(record["user_name"], "employee-42");
    assert_eq!(record["prod_code_line_num"], 1);
    assert_eq!(record["test_code_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(record["test_ai_code_line_accept_num"], 2);
    assert!(record["prod_ai_code_line_gen_num"].is_null());
    assert!(record["test_ai_code_line_gen_num"].is_null());
    assert_eq!(record["changed_file_num"], 2);
    assert_eq!(record["ai_changed_file_num"], 2);
    let languages: Value =
        serde_json::from_str(record["language_stats"].as_str().unwrap()).unwrap();
    assert_eq!(languages["Rust"]["test_code_line_num"], 2);
    assert_eq!(record["ai_breakdown"][0]["agent_type"], "codex");
    let serialized = snapshot.to_string();
    assert!(!serialized.contains("private_source_sentinel"));
    assert!(!serialized.contains("private_commit_sentinel"));
    assert_eq!(snapshot["coverage"]["moss_compatible"], false);

    let target = _dir.path().join("export.json");
    let saved = export(&database, &["--output", target.to_str().unwrap()]);
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    assert!(saved.stdout.is_empty());
    let saved: Value = serde_json::from_slice(&fs::read(target).unwrap()).unwrap();
    assert_eq!(saved["commits"], snapshot["commits"]);

    let config = _dir.path().join("classification.json");
    fs::write(
        &config,
        r#"{"test_patterns":["src/**"],"language_extensions":{"rs":"InternalRust"}}"#,
    )
    .unwrap();
    let custom = exported(&database, &["--config", config.to_str().unwrap()]);
    let record = custom["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["commit"] == commit.commit_sha)
        .unwrap();
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["test_code_line_num"], 1);
    let languages: Value =
        serde_json::from_str(record["language_stats"].as_str().unwrap()).unwrap();
    assert_eq!(languages["InternalRust"]["test_code_line_num"], 1);
}

fn token(session: &str, parent: Option<&str>, seq: u64, input: u64, time: u32) -> MetricEvent {
    let mut attrs = EventAttributes::with_version("test")
        .session_id(session)
        .tool("codex")
        .model("gpt-test");
    if let Some(parent) = parent {
        attrs = attrs.parent_session_id(parent);
    }
    let mut event = MetricEvent::new(
        &TokenUsageValues::new()
            .bucket_ts(1_800_000_000)
            .speed(0)
            .emitted_seq(seq)
            .input_tokens(input)
            .output_tokens(10)
            .cache_read_tokens(20)
            .cache_write_tokens(5)
            .reasoning_output_tokens_opt(Some(3))
            .total_tokens(input + 35),
        attrs.to_sparse(),
    );
    event.timestamp = time;
    event
}

#[test]
fn export_metrics_uses_latest_token_revisions_including_downward_corrections() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = TestRepo::new();
    fs::write(repo.path().join("anchor.txt"), "human anchor\n").unwrap();
    repo.stage_all_and_commit("anchor").unwrap();
    repo.filename("anchor.txt")
        .assert_committed_lines(crate::lines!["human anchor".unattributed_human()]);
    let mut db = MetricsDatabase::open_at_path(Path::new(&database)).unwrap();
    let events = [
        token("root", None, 1, 100, 1_800_000_001),
        token("root", None, 3, 40, 1_800_000_003),
        token("root", None, 2, 900, 1_800_000_010),
        token("child", Some("root"), 1, 20, 1_800_000_005),
        MetricEvent::new(
            &SessionEventValues::new(json!({"text":"private_transcript_sentinel"})),
            EventAttributes::with_version("test")
                .session_id("root")
                .to_sparse(),
        ),
    ];
    db.insert_events(
        &events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let before = db.count().unwrap();
    let output = exported(&database, &[]);
    assert_eq!(output["token_usage"].as_array().unwrap().len(), 2);
    let root = output["token_usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["session_id"] == "root")
        .unwrap();
    assert_eq!(root["input_tokens"], 40);
    assert_eq!(root["emitted_seq"], 3);
    assert_eq!(root["total_tokens"], 75);
    assert_eq!(root["reasoning_tokens"], 3);
    assert_eq!(root["timestamp"], 1_800_000_000_000_u64);
    assert_eq!(root["record_type"], "token_bucket");
    assert_eq!(output["token_root_totals"][0]["input_tokens"], 60);
    assert_eq!(output["token_root_totals"][0]["total_tokens"], 130);
    assert_eq!(
        output["token_root_totals"][0]["descendant_session_count"],
        1
    );
    assert_eq!(
        output["token_root_totals"][0]["aggregation_complete"],
        false
    );
    assert!(!output.to_string().contains("private_transcript_sentinel"));
    assert_eq!(
        before,
        db.count().unwrap(),
        "export must not mark events delivered or remove them"
    );
    let repeated = exported(&database, &[]);
    assert_eq!(output["token_usage"], repeated["token_usage"]);
    assert_eq!(output["token_root_totals"], repeated["token_root_totals"]);
    let filtered = exported(&database, &["--since", "2027-01-16T08:05:00Z"]);
    assert!(filtered["token_usage"].as_array().unwrap().is_empty());
}

#[test]
fn export_metrics_rejects_invalid_config_and_preserves_existing_output() {
    let (dir, database) = isolated_metrics_db_path();
    let _db = MetricsDatabase::open_at_path(Path::new(&database)).unwrap();
    let repo = TestRepo::new();
    fs::write(repo.path().join("anchor.txt"), "human anchor\n").unwrap();
    repo.stage_all_and_commit("anchor").unwrap();
    repo.filename("anchor.txt")
        .assert_committed_lines(crate::lines!["human anchor".unattributed_human()]);
    let config = dir.path().join("config.json");
    fs::write(&config, r#"{"test_patterns":["["]}"#).unwrap();
    assert!(
        !export(&database, &["--config", config.to_str().unwrap()])
            .status
            .success()
    );
    let target = dir.path().join("existing.json");
    fs::write(&target, "user data").unwrap();
    assert!(
        !export(&database, &["--output", target.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "user data");
    let missing = dir.path().join("missing.db");
    assert!(!export(missing.to_str().unwrap(), &[]).status.success());
    assert!(
        !missing.exists(),
        "reading a missing source must not create a database"
    );
    assert!(!export(&database, &["--since", "bad-time"]).status.success());
    assert!(
        !export(
            &database,
            &[
                "--until",
                "1970-01-01T00:00:00Z",
                "--since",
                "2026-01-01T00:00:00Z"
            ]
        )
        .status
        .success()
    );
}

mod rewrite_operations;
