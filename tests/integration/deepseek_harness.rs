use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use mockito::{Matcher, Request};
use serde_json::{Value, json};
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[test]
#[ignore = "requires a DeepSeek Harness CLI"]
fn deepseek_harness_headless_attributes_file_edits() {
    let cli = std::env::var_os("GIT_AI_TEST_DEEPSEEK_HARNESS_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dsh"));
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let repo = TestRepo::new_at_path(&root.join("repo-a"));
    let other = TestRepo::new_at_path(&root.join("repo-b"));
    let initial = "Replace this line.\nAnchor one.\nAnchor two.\nAnchor three.\n";
    fs::write(repo.path().join("existing.txt"), initial).unwrap();
    repo.stage_all_and_commit("Baseline A").unwrap();
    repo.filename("existing.txt").assert_committed_lines(lines![
        "Replace this line.".unattributed_human(),
        "Anchor one.".unattributed_human(),
        "Anchor two.".unattributed_human(),
        "Anchor three.".unattributed_human(),
    ]);
    fs::write(other.path().join("existing.txt"), "Before the tool runs.\n").unwrap();
    other.stage_all_and_commit("Baseline B").unwrap();
    other
        .filename("existing.txt")
        .assert_committed_lines(lines!["Before the tool runs.".unattributed_human(),]);
    fs::write(
        repo.path().join("existing.txt"),
        format!("{initial}Manual pending line.\n"),
    )
    .unwrap();

    let git_ai = repo.git_ai_command_without_pre_sync_for_test(&[], &[]);
    let path = std::env::join_paths(
        [PathBuf::from(git_ai.get_program())
            .parent()
            .unwrap()
            .to_owned()]
        .into_iter()
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let module =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("agent-support/deepseek-harness/index.mjs");
    let overlay = root.join("probe.yml");
    fs::write(
        &overlay,
        format!(
            "- insert:\n  - id: git-ai-deepseek-harness\n    name: {}\n",
            serde_json::to_string(module.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();

    let mut server = mockito::Server::new();
    let _models = server
        .mock("GET", Matcher::Regex("/models$".into()))
        .with_header("content-type", "application/json")
        .with_body(
            json!({"object": "list", "data": [{"id": "deepseek-flash", "object": "model"}]})
                .to_string(),
        )
        .create();
    let completed = Arc::new(Mutex::new(Vec::new()));
    let response_results = Arc::clone(&completed);
    let messages = server
        .mock("POST", Matcher::Regex("/messages$".into()))
        .with_header("content-type", "text/event-stream")
        .with_body_from_request(move |request| {
            scripted_tool_response(request, &response_results).into_bytes()
        })
        .expect_at_least(4)
        .create();

    let output = Command::new(cli)
        .args(["--profile", "headless", "--patch"])
        .arg(overlay)
        .args(["--json", "Apply the test changes."])
        .current_dir(&root)
        .envs(
            git_ai
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("PATH", path)
        .env("DSH_HOME", root.join("Harness home"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("DEEPSEEK_API_KEY", "git-ai-local-test")
        .env("DEEPSEEK_BASE_URL", server.url())
        // Exercise the production receipt flag. TestRepo also sets the legacy test DB variable.
        .env_remove("GIT_AI_TEST_DB_PATH")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Harness stdout:\n{stdout}\nHarness stderr:\n{stderr}"
    );
    messages.assert();
    let results = completed.lock().unwrap();
    assert_eq!(*results, ["write-file", "read-file", "edit-file"]);
    let session = stdout
        .lines()
        .find_map(|line| {
            let event: Value = serde_json::from_str(line).ok()?;
            (event["type"] == "final").then(|| event["sessionId"].as_str().unwrap().to_owned())
        })
        .unwrap();

    repo.sync_daemon();
    other.sync_daemon();
    let first = repo.stage_all_and_commit("Harness edit").unwrap();
    repo.filename("existing.txt").assert_committed_lines(lines![
        "Edited by Harness.".ai(),
        "Anchor one.".unattributed_human(),
        "Anchor two.".unattributed_human(),
        "Anchor three.".unattributed_human(),
        "Manual pending line.".unattributed_human(),
    ]);
    let second = other.stage_all_and_commit("Harness write").unwrap();
    other
        .filename("new/nested/generated.txt")
        .assert_committed_lines(lines!["Generated by Harness.".ai(),]);
    other
        .filename("existing.txt")
        .assert_committed_lines(lines!["Before the tool runs.".unattributed_human(),]);
    for commit in [first, second] {
        let sessions = commit.authorship_log.metadata.sessions;
        assert_eq!(sessions.len(), 1);
        let id = &sessions.values().next().unwrap().agent_id;
        assert_eq!(id.tool, "deepseek-harness");
        assert_eq!(id.id, session);
        assert!(id.model.starts_with("deepseek-"), "model: {}", id.model);
    }
}

fn scripted_tool_response(request: &Request, results: &Mutex<Vec<String>>) -> String {
    let request: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
    let mut observed = results.lock().unwrap();
    *observed = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|item| item["type"] == "tool_result" && item["is_error"] != true)
        .map(|item| item["tool_use_id"].as_str().unwrap().to_owned())
        .collect();
    let calls = [
        (
            "write-file",
            "write",
            json!({"file_path": "repo-b/new/nested/generated.txt", "content": "Generated by Harness.\n"}),
        ),
        (
            "read-file",
            "read",
            json!({"file_path": "repo-a/existing.txt"}),
        ),
        (
            "edit-file",
            "edit",
            json!({"file_path": "repo-a/existing.txt", "old_string": "Replace this line.", "new_string": "Edited by Harness."}),
        ),
    ];

    let mut output = String::new();
    let mut frame = |kind: &str, mut data: Value| {
        data["type"] = json!(kind);
        write!(output, "event: {kind}\ndata: {data}\n\n").unwrap();
    };
    frame(
        "message_start",
        json!({"message": {
            "id": format!("fixture-{}", observed.len()), "type": "message", "role": "assistant",
            "content": [], "model": request["model"], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 2, "output_tokens": 0},
        }}),
    );
    let reason = if let Some((id, name, args)) = calls.get(observed.len()) {
        frame(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        );
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": args.to_string()}}),
        );
        "tool_use"
    } else {
        frame(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        );
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "done"}}),
        );
        "end_turn"
    };
    frame("content_block_stop", json!({"index": 0}));
    frame(
        "message_delta",
        json!({"delta": {"stop_reason": reason, "stop_sequence": null}, "usage": {"output_tokens": 8}}),
    );
    frame("message_stop", json!({}));
    output
}
