use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn parse_agent_v1(hook_input: &str) -> Result<Vec<ParsedHookEvent>, git_ai::error::GitAiError> {
    resolve_preset("agent-v1")?.parse(hook_input, "t_test")
}

#[test]
fn test_agent_v1_human_checkpoint_with_dirty_files() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"],
        "dirty_files": {
            "/Users/test/project/file.ts": "console.log('hello');"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "human");
            assert_eq!(e.context.agent_id.id, "human");
            assert_eq!(e.context.agent_id.model, "human");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file.ts"))
                    .unwrap(),
                "console.log('hello');"
            );
        }
        _ => panic!("Expected PreFileEdit for human checkpoint"),
    }
}

#[test]
fn test_agent_v1_ai_agent_checkpoint_with_dirty_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123",
        "dirty_files": {
            "/Users/test/project/file.ts": "console.log('hello');"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "test-agent");
            assert_eq!(e.context.agent_id.id, "test-123");
            assert_eq!(e.context.agent_id.model, "test-model");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file.ts"))
                    .unwrap(),
                "console.log('hello');"
            );
            assert!(e.stream_source.is_none());
        }
        _ => panic!("Expected PostFileEdit for ai_agent checkpoint"),
    }
}

#[test]
fn test_agent_v1_human_checkpoint_without_dirty_files() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"]
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(e.dirty_files.is_none());
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_agent_v1_ai_agent_checkpoint_without_dirty_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123"
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.dirty_files.is_none());
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            assert_eq!(e.context.agent_id.tool, "test-agent");
            assert_eq!(e.context.agent_id.id, "test-123");
            assert_eq!(e.context.agent_id.model, "test-model");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_dirty_files_multiple_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file1.ts", "/Users/test/project/file2.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123",
        "dirty_files": {
            "/Users/test/project/file1.ts": "content1",
            "/Users/test/project/file2.ts": "content2"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 2);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file1.ts"))
                    .unwrap(),
                "content1"
            );
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file2.ts"))
                    .unwrap(),
                "content2"
            );
            assert_eq!(e.file_paths.len(), 2);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_dirty_files_relative_paths_resolved_to_absolute() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["src/main.rs"],
        "dirty_files": {
            "src/main.rs": "fn main() {}"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/src/main.rs")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(
                dirty_files.contains_key(&PathBuf::from("/Users/test/project/src/main.rs")),
                "dirty_files keys should be resolved to absolute paths"
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }

    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["src/main.rs"],
        "agent_name": "github-copilot-jetbrains",
        "model": "unknown",
        "conversation_id": "session-1",
        "dirty_files": {
            "src/main.rs": "fn main() { println!(\"hello\"); }"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/src/main.rs")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(
                dirty_files.contains_key(&PathBuf::from("/Users/test/project/src/main.rs")),
                "dirty_files keys should be resolved to absolute paths"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_relative_dirty_files_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(crate::lines!["original line".unattributed_human(),]);

    let repo_dir = repo.path().to_string_lossy().to_string();

    let pre_edit_content = "original line\n";
    let human_payload = json!({
        "type": "human",
        "repo_working_dir": repo_dir,
        "will_edit_filepaths": ["test.txt"],
        "dirty_files": {
            "test.txt": pre_edit_content
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &human_payload])
        .unwrap();

    let post_edit_content = "original line\nAI added line\n";
    fs::write(&file_path, post_edit_content).unwrap();

    let ai_payload = json!({
        "type": "ai_agent",
        "repo_working_dir": repo_dir,
        "edited_filepaths": ["test.txt"],
        "agent_name": "github-copilot-jetbrains",
        "model": "unknown",
        "conversation_id": "session-123",
        "dirty_files": {
            "test.txt": post_edit_content
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &ai_payload])
        .unwrap();

    repo.stage_all_and_commit("AI edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "original line".unattributed_human(),
        "AI added line".ai(),
    ]);
}

#[test]
fn test_agent_v1_shell_command_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("script-output.txt");

    fs::write(&file_path, "base line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("script-output.txt");
    file.assert_committed_lines(crate::lines!["base line".unattributed_human(),]);

    let repo_dir = repo.canonical_path().to_string_lossy().to_string();

    let pre_payload = json!({
        "type": "pre_shell_command",
        "repo_working_dir": repo_dir,
        "agent_name": "agent-v1-test",
        "model": "test-model",
        "conversation_id": "shell-session-123",
        "tool_use_id": "shell-tool-1",
        "command": "printf 'created by shell\\n' >> script-output.txt"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &pre_payload])
        .unwrap();

    fs::write(&file_path, "base line\ncreated by shell\n").unwrap();

    let post_payload = json!({
        "type": "post_shell_command",
        "repo_working_dir": repo_dir,
        "agent_name": "agent-v1-test",
        "model": "test-model",
        "conversation_id": "shell-session-123",
        "tool_use_id": "shell-tool-1",
        "command": "printf 'created by shell\\n' >> script-output.txt"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &post_payload])
        .unwrap();

    repo.stage_all_and_commit("Agent v1 shell edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "base line".unattributed_human(),
        "created by shell".ai(),
    ]);
}

#[test]
fn test_agent_v1_transcript_is_readable_from_commit_and_private_to_local_store() {
    use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
    use serde_json::Value;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(json!({"checkpoint_debug_log": true}));
    });
    let file = repo.path().join("transcript.txt");
    fs::write(&file, "manual line\n").unwrap();
    repo.stage_all_and_commit("Seed tracked file").unwrap();
    repo.filename("transcript.txt")
        .assert_committed_lines(lines!["manual line".unattributed_human(),]);

    let input = |payload: Value| {
        repo.git_ai_with_stdin(
            &[
                "checkpoint",
                "agent-v1",
                "--hook-input",
                "stdin",
                "--print-receipt",
            ],
            payload.to_string().as_bytes(),
        )
        .unwrap()
    };
    input(json!({
        "type": "human",
        "repo_working_dir": repo.canonical_path(),
        "will_edit_filepaths": ["transcript.txt"],
    }));
    fs::write(&file, "manual line\nAI line\n").unwrap();
    let marker = "Local-only fixture prompt";
    let token = "sk_test_4eC39HqLyjWDarjtT1zdp7dc";
    input(json!({
        "type": "ai_agent",
        "repo_working_dir": repo.canonical_path(),
        "edited_filepaths": ["transcript.txt"],
        "agent_name": "deepseek-harness",
        "model": "test-model",
        "conversation_id": "agent-v1-transcript-test",
        "transcript": {"messages": [
            {"type": "user", "text": marker},
            {"type": "assistant", "text": format!("Fixture key: {token}")},
            {"type": "tool_use", "name": "write", "input": {"file_path": "transcript.txt"}},
        ]},
    }));
    let commit = repo.stage_all_and_commit("Attach transcript").unwrap();
    repo.filename("transcript.txt")
        .assert_committed_lines(lines!["manual line".unattributed_human(), "AI line".ai(),]);

    let note = repo.read_authorship_note(&commit.commit_sha).unwrap();
    assert!(!note.contains(marker));
    let note = AuthorshipLog::deserialize_from_string(&note).unwrap();
    let attestation = &note.attestations[0]
        .entries
        .iter()
        .find(|entry| entry.hash.starts_with("s_"))
        .unwrap()
        .hash;
    let output = repo
        .git_ai(&["show-prompt", attestation, "--commit", &commit.commit_sha])
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["transcript"]["messages"][0]["text"], marker);
    let reply = output["transcript"]["messages"][1]["text"]
        .as_str()
        .unwrap();
    assert!(reply.starts_with("Fixture key: "));
    assert!(!reply.contains(token));
    assert_eq!(output["transcript"]["messages"][2]["name"], "write");

    let log_dir = repo
        .test_home_path()
        .join(".git-ai/internal/checkpoint-debug-logs");
    let logs = fs::read_dir(log_dir)
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(logs.contains("agent-v1-transcript-test"));
    assert!(!logs.contains(marker));
    assert!(!logs.contains(token));
    for entry in logs
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
    {
        let input: Value = serde_json::from_str(entry["hook_input"].as_str().unwrap()).unwrap();
        assert!(input.get("transcript").is_none());
        assert!(
            entry["requests"]
                .as_array()
                .unwrap()
                .iter()
                .all(|request| request.get("transcript").is_none())
        );
    }
}

#[test]
fn test_agent_v1_omits_oversized_transcript_without_losing_the_edit() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/project",
        "edited_filepaths": ["test.txt"],
        "agent_name": "deepseek-harness",
        "model": "test-model",
        "conversation_id": "agent-v1-long-transcript-test",
        "transcript": {"messages": [{"type": "user", "text": "a".repeat(512 * 1024)}]},
    })
    .to_string();
    let events = parse_agent_v1(&hook_input).unwrap();
    assert!(
        matches!(events.as_slice(), [ParsedHookEvent::PostFileEdit(edit)] if edit.file_paths.ends_with(&[PathBuf::from("/project/test.txt")]))
    );
}
