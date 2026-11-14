use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::commands::checkpoint_agent::presets::resolve_preset;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const SESSION: &str = "session_git_ai_fixture";
const FILE: &str = "src/example.txt";

fn checkpoint(repo: &TestRepo, agent: &str, payload: &Value) -> String {
    repo.git_ai_with_stdin(
        &[
            "checkpoint",
            agent,
            "--hook-input",
            "stdin",
            "--print-receipt",
        ],
        payload.to_string().as_bytes(),
    )
    .unwrap()
}

fn append_jsonl(path: &Path, rows: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

fn hermes_home(home: &Path) -> PathBuf {
    if cfg!(windows) {
        home.join("AppData/Local/hermes")
    } else {
        home.join(".hermes")
    }
}

fn exercise_two_turns(
    repo: &TestRepo,
    agent: &str,
    tool: &str,
    model: &str,
    mut hook: impl FnMut(usize, &str, &str, bool) -> Value,
) {
    let path = repo.path().join(FILE);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "original line\n").unwrap();
    repo.stage_all_and_commit("Seed the repository").unwrap();
    repo.filename(FILE)
        .assert_committed_lines(lines!["original line".unattributed_human()]);

    let mut notes = Vec::new();
    let mut source = String::from("original line\n");
    for turn in 1..=2 {
        let prompt = format!("fixture prompt {turn}");
        let reply = format!("fixture reply {turn}");
        source.insert_str(0, &format!("manual baseline {turn}\n"));
        fs::write(&path, &source).unwrap();
        let pre = checkpoint(repo, agent, &hook(turn, &prompt, &reply, false));
        assert!(pre.contains("checkpoint_requests=1"), "{agent}: {pre}");
        let working_log = repo.current_working_logs();
        let checkpoints = working_log.read_all_checkpoints().unwrap();
        let baseline = checkpoints
            .last()
            .and_then(|checkpoint| checkpoint.entries.first());
        assert!(
            baseline.is_some_and(|entry| working_log
                .get_file_version(&entry.blob_sha)
                .ok()
                .as_deref()
                == Some(source.as_str())),
            "{agent}: pre-edit checkpoint did not save the baseline: {checkpoints:#?}",
        );
        source.push_str(&format!("agent edit {turn}\n"));
        fs::write(&path, &source).unwrap();
        let post = checkpoint(repo, agent, &hook(turn, &prompt, &reply, true));
        assert!(post.contains("checkpoint_requests=1"), "{agent}: {post}");
        let commit = repo.stage_all_and_commit(&format!("Turn {turn}")).unwrap();
        if turn == 1 {
            repo.filename(FILE).assert_committed_lines(lines![
                "manual baseline 1".unattributed_human(),
                "original line".unattributed_human(),
                "agent edit 1".ai(),
            ]);
        } else {
            repo.filename(FILE).assert_committed_lines(lines![
                "manual baseline 2".unattributed_human(),
                "manual baseline 1".unattributed_human(),
                "original line".unattributed_human(),
                "agent edit 1".ai(),
                "agent edit 2".ai(),
            ]);
        }
        let note = repo.read_authorship_note(&commit.commit_sha).unwrap();
        assert!(!note.contains(&prompt));
        let note = AuthorshipLog::deserialize_from_string(&note).unwrap();
        let attestation = note
            .attestations
            .iter()
            .flat_map(|file| &file.entries)
            .find(|entry| {
                entry.hash.starts_with("s_")
                    && entry
                        .line_ranges
                        .iter()
                        .any(|range| range.contains(source.lines().count() as u32))
            })
            .unwrap();
        notes.push((commit.commit_sha, attestation.hash.clone()));
    }

    assert_ne!(notes[0].1, notes[1].1);
    for (index, (sha, attestation)) in notes.iter().enumerate() {
        let output = repo
            .git_ai(&["show-prompt", attestation, "--commit", sha])
            .unwrap();
        let result: Value = serde_json::from_str(&output).unwrap();
        let turn = index + 1;
        assert_eq!(result["prompt"]["agent_id"]["tool"], agent);
        assert_eq!(result["prompt"]["agent_id"]["id"], SESSION);
        assert_eq!(result["prompt"]["agent_id"]["model"], model);
        let messages = result["transcript"]["messages"].as_array().unwrap();
        let prompts: Vec<_> = messages
            .iter()
            .filter(|m| m["type"] == "user")
            .map(|m| m["text"].as_str().unwrap())
            .collect();
        let expected: Vec<_> = (1..=turn).map(|n| format!("fixture prompt {n}")).collect();
        assert_eq!(prompts, expected);
        let reply_turn = if agent == "trae" { turn - 1 } else { turn };
        if reply_turn > 0 {
            assert!(
                messages.iter().any(|m| m["type"] == "assistant"
                    && m["text"] == format!("fixture reply {reply_turn}"))
            );
        }
        assert!(
            messages
                .iter()
                .any(|m| m["type"] == "tool_use" && m["name"] == tool)
        );
        assert!(!output.contains("fixture tool result"));
    }
}

#[test]
fn qoder_commit_transcripts() {
    let repo = TestRepo::new();
    let transcript = repo.test_home_path().join("qoder-session.jsonl");
    exercise_two_turns(
        &repo,
        "qoder",
        "search_replace",
        "qoder-fixture",
        |turn, prompt, reply, post| {
            if post {
                append_jsonl(
                    &transcript,
                    &[
                        json!({"type":"user","message":{"role":"user","content":prompt}}),
                        json!({"type":"assistant","message":{"role":"assistant","model":"qoder-fixture","content":[
                            {"type":"text","text":reply},
                            {"type":"tool_use","id":format!("edit-{turn}"),"name":"search_replace","input":{"file_path":FILE}}
                        ]}}),
                        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"fixture tool result"}]}}),
                    ],
                );
            }
            json!({
                "session_id":SESSION,"cwd":repo.canonical_path(),"transcript_path":transcript,
                "hook_event_name":if post { "PostToolUse" } else { "PreToolUse" },
                "tool_name":"search_replace","tool_use_id":format!("edit-{turn}"),"tool_input":{"file_path":FILE}
            })
        },
    );
}

#[test]
fn hermes_commit_transcripts() {
    let repo = TestRepo::new();
    let dir = hermes_home(repo.test_home_path());
    fs::create_dir(&dir).unwrap();
    let db = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT);
         CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, tool_calls TEXT);",
    ).unwrap();
    db.execute(
        "INSERT INTO sessions VALUES (?1, 'hermes-fixture')",
        [SESSION],
    )
    .unwrap();
    exercise_two_turns(
        &repo,
        "hermes",
        "write_file",
        "hermes-fixture",
        |turn, prompt, reply, post| {
            let id = format!("edit-{turn}");
            if post {
                let calls = json!([{"id":id,"type":"function","function":{"name":"write_file","arguments":json!({"path":FILE}).to_string()}}]);
                for (role, text, calls) in [
                    ("user", prompt, None),
                    ("assistant", reply, Some(calls.to_string())),
                    ("tool", "fixture tool result", None),
                ] {
                    db.execute(
                    "INSERT INTO messages (session_id, role, content, tool_calls) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![SESSION, role, text, calls],
                ).unwrap();
                }
            }
            json!({
                "session_id":SESSION,"cwd":repo.canonical_path(),"profile":"default",
                "hook_event_name":if post { "post_tool_call" } else { "pre_tool_call" },
                "tool_name":"write_file","tool_input":{"path":FILE},
                "extra":{"tool_call_id":id,"status":if post {Some("ok")} else {None},
                    "result":json!({"files_modified":[repo.canonical_path().join(FILE)]}).to_string()}
            })
        },
    );
}

#[test]
fn hermes_requires_a_pre_snapshot_of_the_actual_target() {
    let launch = TestRepo::new();
    let repo = TestRepo::new();
    let path = repo.canonical_path().join("target.txt");
    fs::write(launch.path().join("target.txt"), "other repository\n").unwrap();
    fs::write(&path, "baseline\n").unwrap();
    repo.stage_all_and_commit("Seed target").unwrap();
    repo.filename("target.txt")
        .assert_committed_lines(lines!["baseline".unattributed_human()]);

    let edit = |receiver: &TestRepo, event: &str, id: &str, cwd: &Path| {
        checkpoint(
            receiver,
            "hermes",
            &json!({
                "hook_event_name": event, "session_id": SESSION, "cwd": cwd,
                "tool_name": "write_file", "tool_input": {"path": "target.txt"},
                "extra": {"tool_call_id": id, "result": {"resolved_path": path}},
            }),
        )
    };
    fs::write(&path, "baseline\nmanual before edit\n").unwrap();
    edit(
        &launch,
        "pre_tool_call",
        "wrong-path",
        &launch.canonical_path(),
    );
    launch.sync_daemon_force();
    fs::write(&path, "baseline\nmanual before edit\nunpaired edit\n").unwrap();
    edit(
        &repo,
        "post_tool_call",
        "wrong-path",
        &launch.canonical_path(),
    );
    repo.stage_all_and_commit("Unpaired write").unwrap();
    repo.filename("target.txt").assert_committed_lines(lines![
        "baseline".unattributed_human(),
        "manual before edit".unattributed_human(),
        "unpaired edit".unattributed_human(),
    ]);

    edit(
        &repo,
        "pre_tool_call",
        "correct-path",
        &repo.canonical_path(),
    );
    fs::write(
        &path,
        "baseline\nmanual before edit\nunpaired edit\npaired edit\n",
    )
    .unwrap();
    edit(
        &repo,
        "post_tool_call",
        "correct-path",
        &launch.canonical_path(),
    );
    repo.stage_all_and_commit("Paired write").unwrap();
    repo.filename("target.txt").assert_committed_lines(lines![
        "baseline".unattributed_human(),
        "manual before edit".unattributed_human(),
        "unpaired edit".unattributed_human(),
        "paired edit".ai(),
    ]);
}

#[test]
fn trae_commit_transcripts() {
    exercise_trae(None);
}

#[test]
fn trae_secondary_workspace_commit_transcripts() {
    for profile in [".trae-cn", ".trae"] {
        let primary = TestRepo::new();
        exercise_trae(Some((&primary, profile)));
    }
}

#[test]
fn trae_mixed_workspace_paths_respect_the_repository_allowlist() {
    let excluded = TestRepo::new();
    let repo = TestRepo::new();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://git.xiaojukeji.com/example/allowed.git",
    ])
    .unwrap();
    repo.git_ai(&[
        "config",
        "set",
        "allow_repositories",
        "*git.xiaojukeji.com/*",
    ])
    .unwrap();
    let target = repo.canonical_path();
    let other = excluded.canonical_path();
    let path = target.join("file.txt");
    let other_file = other.join("file.txt");
    checkpoint(
        &repo,
        "trae",
        &json!({
            "session_id": SESSION, "cwd": other, "workspace_roots": [other, target],
            "hook_event_name": "UserPromptSubmit", "prompt": "edit both roots",
        }),
    );
    let mut event = json!({
        "session_id": SESSION, "cwd": other, "workspace_roots": [other, target],
        "hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_use_id": "both-roots",
        "tool_input": {"file_paths": [other_file, path]},
    });
    assert!(checkpoint(&repo, "trae", &event).contains("checkpoint_requests=1"));
    fs::write(&path, "allowed edit\n").unwrap();
    fs::write(&other_file, "excluded edit\n").unwrap();
    event["hook_event_name"] = json!("PostToolUse");
    assert!(checkpoint(&repo, "trae", &event).contains("checkpoint_requests=1"));
    assert!(!excluded.path().join(".git/ai/trae-transcripts").exists());
    let commit = repo
        .stage_all_and_commit("Edit in allowed workspace")
        .unwrap();
    repo.filename("file.txt")
        .assert_committed_lines(lines!["allowed edit".ai()]);
    let note = AuthorshipLog::deserialize_from_string(
        &repo.read_authorship_note(&commit.commit_sha).unwrap(),
    )
    .unwrap();
    let attestation = &note.attestations[0].entries[0].hash;
    let output = repo
        .git_ai(&["show-prompt", attestation, "--commit", &commit.commit_sha])
        .unwrap();
    let result: Value = serde_json::from_str(&output).unwrap();
    let messages = result["transcript"]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["text"], "edit both roots");
    assert_eq!(messages[1]["input"]["file_paths"], json!([path]));
}

fn exercise_trae(workspace: Option<(&TestRepo, &str)>) {
    let repo = TestRepo::new();
    let target = repo.canonical_path();
    let primary = workspace.map_or_else(|| target.clone(), |(repo, _)| repo.canonical_path());
    let roots = if workspace.is_some() {
        vec![primary.clone(), target.clone()]
    } else {
        vec![target.clone()]
    };
    if let Some((_, profile)) = workspace {
        let dir = repo.test_home_path().join(profile);
        fs::create_dir_all(&dir).unwrap();
        repo.git_ai(&["install"]).unwrap();
        let config: Value =
            serde_json::from_slice(&fs::read(dir.join("hooks.json")).unwrap()).unwrap();
        assert!(
            config["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("checkpoint trae --hook-input stdin")
        );
    }
    let mut started_turn = 0;
    exercise_two_turns(
        &repo,
        "trae",
        "Edit",
        "unknown",
        |turn, prompt, _reply, post| {
            if !post && turn != started_turn {
                if turn > 1 {
                    checkpoint(
                        &repo,
                        "trae",
                        &json!({
                            "session_id":SESSION,"cwd":primary,"workspace_roots":roots,"hook_event_name":"Stop",
                            "last_assistant_message":format!("fixture reply {}", turn - 1),
                        }),
                    );
                }
                let output = checkpoint(
                    &repo,
                    "trae",
                    &json!({
                        "session_id":SESSION,"cwd":primary,"workspace_roots":roots,"hook_event_name":"UserPromptSubmit","prompt":prompt,
                    }),
                );
                assert!(output.is_empty(), "{output}");
                started_turn = turn;
            }
            json!({
                "session_id":SESSION,"cwd":primary,"workspace_roots":roots,"hook_event_name":if post {"PostToolUse"} else {"PreToolUse"},
                "tool_name":"Edit","llm_tool_name":"Edit","tool_use_id":format!("edit-{turn}"),
                "tool_input":{"file_path":target.join(FILE)},"tool_response":{"output":"fixture tool result"}
            })
        },
    );
}

#[test]
fn kimi_commit_transcripts() {
    exercise_kimi(false);
}

#[test]
fn kimi_stop_attributes_a_write_when_the_post_hook_does_not_run() {
    exercise_kimi(true);
}

#[test]
fn kimi_write_checkpoint_deduplicates_post_and_stop_and_preserves_manual_lines() {
    for (first, second, inserted_before) in [
        ("PostToolUse", "Stop", false),
        ("Stop", "PostToolUse", true),
        ("Stop", "Stop", false),
        ("PostToolUse", "PostToolUse", false),
    ] {
        let repo = TestRepo::new();
        let path = repo.canonical_path().join("write.txt");
        fs::write(&path, "baseline\n").unwrap();
        repo.stage_all_and_commit("Seed Kimi target").unwrap();
        repo.filename("write.txt")
            .assert_committed_lines(lines!["baseline".unattributed_human()]);

        let home = repo.test_home_path().join(".kimi-code");
        let session_dir = home.join("sessions/wd_fixture").join(SESSION);
        append_jsonl(
            &home.join("session_index.jsonl"),
            &[json!({"sessionId": SESSION, "sessionDir": session_dir})],
        );
        let wire = session_dir.join("agents/main/wire.jsonl");
        append_jsonl(
            &wire,
            &[
                json!({"type":"environment.set_binding","environmentId":"local"}),
                json!({"type":"turn.prompt","turnId":1}),
            ],
        );
        let before = "baseline\nmanual before write\n";
        let written = "baseline\nmanual before write\nagent write\n";
        let input = json!({"path":"write.txt","content":written});
        let invoke = |event: &str| {
            let mut payload =
                json!({"session_id":SESSION,"cwd":repo.canonical_path(),"hook_event_name":event});
            if event != "Stop" {
                payload["tool_name"] = json!("Write");
                payload["tool_call_id"] = json!("write-1");
                payload["tool_input"] = input.clone();
            }
            checkpoint(&repo, "kimi-code", &payload)
        };
        fs::write(&path, before).unwrap();
        invoke("PreToolUse");
        fs::write(&path, written).unwrap();
        append_jsonl(
            &wire,
            &[
                json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"write-1","name":"Write","display":{"path":path},"args":input}}),
                json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"write-1","result":{"output":"wrote file"}}}),
            ],
        );

        if first == "PostToolUse" && second == "Stop" {
            invoke(first);
        }
        let manual = if inserted_before {
            "baseline\nmanual before write\nmanual after write\nagent write\n".to_owned()
        } else {
            format!("{written}manual after write\n")
        };
        fs::write(&path, manual).unwrap();
        if (first, second) != ("PostToolUse", "Stop") {
            invoke(first);
        }
        if second == "PostToolUse" {
            append_jsonl(&wire, &[json!({"type":"turn.ended","turnId":1})]);
        }
        invoke(second);
        let log = repo.current_working_logs();
        let checkpoints = log.read_all_checkpoints().unwrap();
        let commit = repo
            .stage_all_and_commit("Kimi write and manual edit")
            .unwrap();
        let expected = if inserted_before {
            lines![
                "baseline".unattributed_human(),
                "manual before write".unattributed_human(),
                "manual after write".unattributed_human(),
                "agent write".ai()
            ]
        } else {
            lines![
                "baseline".unattributed_human(),
                "manual before write".unattributed_human(),
                "agent write".ai(),
                "manual after write".unattributed_human()
            ]
        };
        repo.filename("write.txt").assert_committed_lines(expected);
        let note = repo.read_authorship_note(&commit.commit_sha).unwrap();
        let note = AuthorshipLog::deserialize_from_string(&note).unwrap();
        let lines: Vec<_> = note
            .attestations
            .iter()
            .filter(|file| file.file_path == "write.txt")
            .flat_map(|file| &file.entries)
            .filter(|entry| entry.hash.starts_with("s_"))
            .flat_map(|entry| entry.line_ranges.iter().flat_map(|range| range.expand()))
            .collect();
        assert_eq!(lines, [if inserted_before { 4 } else { 3 }]);
        let captured: Vec<_> = checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.kind.is_ai())
            .collect();
        assert_eq!(captured.len(), 1, "{first} then {second}");
    }
}

#[test]
fn kimi_stop_captures_a_write_before_the_next_pre_on_the_same_file() {
    let repo = TestRepo::new();
    let path = repo.canonical_path().join("write.txt");
    let home = repo.test_home_path().join(".kimi-code");
    let session_dir = home.join("sessions/wd_fixture").join(SESSION);
    append_jsonl(
        &home.join("session_index.jsonl"),
        &[json!({"sessionId": SESSION, "sessionDir": session_dir})],
    );
    let wire = session_dir.join("agents/main/wire.jsonl");
    append_jsonl(
        &wire,
        &[
            json!({"type":"runtime.set_binding","runtimeId":"local"}),
            json!({"type":"turn.prompt","turnId":1}),
        ],
    );
    fs::write(&path, "baseline\n").unwrap();
    repo.stage_all_and_commit("Seed Kimi target").unwrap();
    repo.filename("write.txt")
        .assert_committed_lines(lines!["baseline".unattributed_human()]);

    let pre = |id: &str, content: &str| {
        let input = json!({"path":"write.txt","content":content});
        checkpoint(
            &repo,
            "kimi-code",
            &json!({
                "session_id":SESSION,"cwd":repo.canonical_path(),"hook_event_name":"PreToolUse",
                "tool_name":"Write","tool_call_id":id,"tool_input":input,
            }),
        );
        input
    };
    let complete = |id: &str, input: Value| {
        fs::write(&path, input["content"].as_str().unwrap()).unwrap();
        append_jsonl(
            &wire,
            &[
                json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":id,"name":"Write","display":{"path":path},"args":input}}),
                json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":id,"result":{"output":"wrote file"}}}),
            ],
        );
    };
    complete("first", pre("first", "baseline\nfirst AI edit\n"));
    fs::write(&path, "baseline\nfirst AI edit\nmanual between edits\n").unwrap();
    complete(
        "second",
        pre(
            "second",
            "baseline\nfirst AI edit\nmanual between edits\nsecond AI edit\n",
        ),
    );
    checkpoint(
        &repo,
        "kimi-code",
        &json!({
            "session_id":SESSION,"cwd":repo.canonical_path(),"hook_event_name":"Stop",
        }),
    );
    repo.stage_all_and_commit("Two writes in one turn").unwrap();
    repo.filename("write.txt").assert_committed_lines(lines![
        "baseline".unattributed_human(),
        "first AI edit".ai(),
        "manual between edits".unattributed_human(),
        "second AI edit".ai(),
    ]);
}

fn exercise_kimi(stop_without_post: bool) {
    let repo = TestRepo::new();
    let home = repo.test_home_path().join(".kimi-code");
    let session_dir = home.join("sessions/wd_fixture").join(SESSION);
    let transcript = session_dir.join("agents/main/wire.jsonl");
    let tool = if stop_without_post { "Write" } else { "Edit" };
    append_jsonl(
        &home.join("session_index.jsonl"),
        &[json!({
            "sessionId":SESSION,"sessionDir":session_dir,"workDir":repo.canonical_path(),
        })],
    );
    append_jsonl(
        &transcript,
        &[json!({"type":"runtime.set_binding","runtimeId":"local"})],
    );
    exercise_two_turns(
        &repo,
        "kimi-code",
        tool,
        "kimi-fixture",
        |turn, prompt, reply, post| {
            if !post && turn > 1 {
                append_jsonl(
                    &transcript,
                    &[json!({"type":"turn.ended","turnId":turn - 1})],
                );
            }
            let input = if stop_without_post {
                json!({"path":FILE,"mode":"append","content":format!("agent edit {turn}\n")})
            } else {
                json!({"path":FILE})
            };
            if post {
                append_jsonl(
                    &transcript,
                    &[
                        json!({"type":"turn.prompt","turnId":turn}),
                        json!({"type":"context.append_message","message":{"role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":prompt}],"toolCalls":[]}}),
                        json!({"type":"llm.request","model":"kimi-fixture"}),
                        json!({"type":"context.append_message","message":{"role":"user","origin":{"kind":"injection"},"content":[{"type":"text","text":"fixture system reminder"}],"toolCalls":[]}}),
                        json!({"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":reply}}}),
                        json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":format!("edit-{turn}"),"name":tool,"display":{"path":repo.canonical_path().join(FILE)},"args":input}}),
                        json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":format!("edit-{turn}"),"result":{"output":"fixture tool result"}}}),
                        json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":format!("failed-{turn}"),"name":"Write","display":{"path":repo.canonical_path().join("failed-kimi-edit.txt")},"args":{"path":"failed-kimi-edit.txt","content":"failed write"}}}),
                        json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":format!("failed-{turn}"),"result":{"isError":true,"output":"fixture tool result"}}}),
                    ],
                );
            }
            if post && stop_without_post {
                return json!({
                    "session_id":SESSION,"cwd":repo.canonical_path(),"client_type":"kimi_code_cli",
                    "hook_event_name":"Stop","stop_hook_active":false,
                });
            }
            json!({
                "session_id":SESSION,"cwd":repo.canonical_path(),"client_type":"kimi_code_cli",
                "hook_event_name":if post {"PostToolUse"} else {"PreToolUse"},
                "tool_name":tool,"tool_call_id":format!("edit-{turn}"),"tool_input":input,
            })
        },
    );
}

#[test]
fn ignores_read_only_and_failed_edit_events() {
    for (agent, event, tool, extra) in [
        ("qoder", "PostToolUse", "Read", json!(null)),
        (
            "hermes",
            "post_tool_call",
            "write_file",
            json!({"status":"blocked"}),
        ),
        ("trae", "PostToolUse", "Read", json!(null)),
        ("kimi-code", "PostToolUseFailure", "Edit", json!(null)),
    ] {
        let payload = json!({
            "session_id":SESSION,"cwd":std::env::current_dir().unwrap(),"hook_event_name":event,
            "tool_name":tool,"tool_input":{"path":FILE},"extra":extra
        });
        assert!(
            resolve_preset(agent)
                .unwrap()
                .parse(&payload.to_string(), "test")
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn install_and_uninstall_preserve_user_hooks() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    let custom_command = "echo /tmp/git-ai checkpoint qoder --hook-input stdin";
    let paths: [(&str, PathBuf, &str); 5] = [
        (
            "qoder",
            home.join(".qoder/settings.json"),
            r#"{"custom":true,"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo user-hook"},{"type":"command","command":"echo /tmp/git-ai checkpoint qoder --hook-input stdin"}]}]}}"#,
        ),
        (
            "trae",
            home.join(".trae-cn/hooks.json"),
            r#"{"version":1,"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo user-hook"}]}]}}"#,
        ),
        (
            "trae",
            home.join(".trae/hooks.json"),
            r#"{"version":1,"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"echo user-hook"}]}]}}"#,
        ),
        (
            "hermes",
            hermes_home(home).join("config.yaml"),
            "# preserve comment\nmodel: fixture\nplugins:\n  enabled: [user-plugin]\nhooks:\n  pre_tool_call:\n    - command: echo user-hook\n    - command: /old/git-ai checkpoint hermes --hook-input stdin\n",
        ),
        (
            "kimi-code",
            home.join(".kimi-code/config.toml"),
            "# preserve comment\ndefault_model = 'fixture'\n\n[[hooks]]\nevent = 'PreToolUse'\ncommand = 'echo user-hook'\n",
        ),
    ];
    for (_, path, config) in &paths {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, config).unwrap();
    }

    repo.git_ai(&["install", "--dry-run"]).unwrap();
    for (_, path, config) in &paths {
        assert_eq!(fs::read_to_string(path).unwrap(), *config);
    }
    repo.git_ai(&["install"]).unwrap();
    let installed: Vec<_> = paths
        .iter()
        .map(|(agent, path, _)| {
            let content = fs::read_to_string(path).unwrap();
            let command = format!("checkpoint {agent} --hook-input stdin");
            let expected = match *agent {
                "trae" => 4,
                "kimi-code" => 3,
                "hermes" => 0,
                _ => 2,
            };
            assert_eq!(
                content
                    .replace(custom_command, "")
                    .matches(&command)
                    .count(),
                expected,
                "{}",
                path.display()
            );
            assert!(content.contains("echo user-hook"));
            if *agent == "qoder" {
                assert!(content.contains(custom_command));
            }
            if *agent == "hermes" {
                assert!(content.contains("user-plugin"));
                assert_eq!(content.matches("git-ai-hooks").count(), 1);
                let plugin = hermes_home(home).join("plugins/git-ai-hooks");
                assert!(plugin.join("__init__.py").is_file());
                assert!(plugin.join("plugin.yaml").is_file());
            }
            if matches!(*agent, "hermes" | "kimi-code") {
                assert!(content.contains("# preserve comment"));
            }
            content
        })
        .collect();
    repo.git_ai(&["install"]).unwrap();
    for ((_, path, _), expected) in paths.iter().zip(installed) {
        assert_eq!(fs::read_to_string(path).unwrap(), expected);
    }
    repo.git_ai(&["uninstall-hooks"]).unwrap();
    for (agent, path, _) in &paths {
        let content = fs::read_to_string(path).unwrap();
        if *agent == "qoder" {
            assert!(content.contains(custom_command));
        }
        if *agent == "hermes" {
            assert!(content.contains("user-plugin"));
            assert!(!content.contains("git-ai-hooks"));
            assert!(
                !hermes_home(home)
                    .join("plugins/git-ai-hooks/__init__.py")
                    .exists()
            );
        }
        assert!(
            !content
                .replace(custom_command, "")
                .contains(&format!("checkpoint {agent} --hook-input stdin"))
        );
        assert!(content.contains("echo user-hook"));
    }
}

#[test]
fn install_detects_each_trae_edition() {
    for (name, dir) in [("TraeCode CN", ".trae-cn"), ("TraeCode", ".trae")] {
        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
        let home = repo.test_home_path();
        if cfg!(target_os = "macos") {
            fs::create_dir_all(home.join("Applications").join(format!("{name}.app"))).unwrap();
        } else {
            let base = if cfg!(windows) {
                "AppData/Roaming"
            } else {
                ".config"
            };
            let settings = home.join(base).join(name).join("User/settings.json");
            fs::create_dir_all(settings.parent().unwrap()).unwrap();
            fs::write(settings, "{}").unwrap();
        }
        repo.git_ai(&["install"]).unwrap();
        let path = home.join(dir).join("hooks.json");
        let config =
            fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let trae: Value = serde_json::from_str(&config).unwrap();
        for event in ["PreToolUse", "PostToolUse", "UserPromptSubmit", "Stop"] {
            assert!(
                trae["hooks"][event][0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .ends_with(" checkpoint trae --hook-input stdin"),
                "{name}: {event}"
            );
        }
    }
}

#[test]
fn install_creates_fresh_agent_configs() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let home = repo.test_home_path();
    fs::create_dir_all(hermes_home(home)).unwrap();
    for dir in [".qoder", ".kimi-code", ".pi", ".config/opencode"] {
        fs::create_dir_all(home.join(dir)).unwrap();
    }
    repo.git_ai(&["install"]).unwrap();
    assert!(home.join(".pi/agent/extensions/git-ai.ts").is_file());
    assert!(home.join(".config/opencode/plugins/git-ai.ts").is_file());
    let qoder: Value =
        serde_json::from_str(&fs::read_to_string(home.join(".qoder/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        qoder["hooks"]["PostToolUse"][0]["hooks"][0]["type"],
        "command"
    );
    let kimi: toml::Value =
        toml::from_str(&fs::read_to_string(home.join(".kimi-code/config.toml")).unwrap()).unwrap();
    assert_eq!(kimi["hooks"].as_array().unwrap().len(), 3);
    let hermes: yaml_edit::Document = fs::read_to_string(hermes_home(home).join("config.yaml"))
        .unwrap()
        .parse()
        .unwrap();
    let plugins = hermes.get_mapping("plugins").unwrap();
    let enabled = plugins.get_sequence("enabled").unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(
        enabled
            .values()
            .next()
            .unwrap()
            .as_scalar()
            .unwrap()
            .as_string(),
        "git-ai-hooks"
    );
    assert!(!home.join(".kimi").exists());
}
