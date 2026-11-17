use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, get_binary_path};
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires DSH_ROOT pointing to a deepseek-harness checkout with dependencies installed"]
fn deepseek_harness_attributes_tool_edits_in_two_repositories() {
    const BASE_EDIT: &str =
        "base edit\nunchanged one\nunchanged two\nunchanged three\nunchanged four\n";
    const SESSION_ID: &str = "dsh-git-ai-e2e";

    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let child_a = root.join("child-a");
    let child_b = root.join("child-b");
    let repo_a = TestRepo::new_at_path(&child_a);
    let repo_b = TestRepo::new_at_path(&child_b);

    fs::write(child_a.join("dsh-edit.txt"), BASE_EDIT).unwrap();
    fs::write(child_b.join("dsh-bash.txt"), "base bash\n").unwrap();
    repo_a.stage_all_and_commit("Baseline A").unwrap();
    repo_a
        .filename("dsh-edit.txt")
        .assert_committed_lines(lines![
            "base edit".unattributed_human(),
            "unchanged one".unattributed_human(),
            "unchanged two".unattributed_human(),
            "unchanged three".unattributed_human(),
            "unchanged four".unattributed_human(),
        ]);
    repo_b.stage_all_and_commit("Baseline B").unwrap();
    repo_b
        .filename("dsh-bash.txt")
        .assert_committed_lines(lines!["base bash".unattributed_human()]);

    fs::write(
        child_a.join("dsh-edit.txt"),
        format!("{BASE_EDIT}human pending\n"),
    )
    .unwrap();

    let runner =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("agent-support/deepseek-harness/test/run.mjs");
    let git_ai_command = repo_a.git_ai_command_without_pre_sync_for_test(&[], &[]);
    let output = Command::new("node")
        .arg(runner)
        .envs(
            git_ai_command
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("DSH_GIT_AI_WORKSPACE", &root)
        .env("DSH_GIT_AI_BIN", get_binary_path())
        .env("DSH_GIT_AI_SESSION", SESSION_ID)
        .output()
        .expect("start DeepSeek Harness test runner");
    // The runner can succeed even if an optional checkpoint fails.
    eprintln!(
        "DeepSeek Harness runner stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "DeepSeek Harness runner failed ({})",
        output.status,
    );

    repo_a.sync_daemon();
    repo_b.sync_daemon();
    let commit_a = repo_a
        .stage_all_and_commit("Harness write and edit")
        .unwrap();
    repo_a
        .filename("dsh-write.txt")
        .assert_committed_lines(lines!["AI write".ai()]);
    repo_a
        .filename("dsh-edit.txt")
        .assert_committed_lines(lines![
            "AI edit".ai(),
            "unchanged one".unattributed_human(),
            "unchanged two".unattributed_human(),
            "unchanged three".unattributed_human(),
            "unchanged four".unattributed_human(),
            "human pending".unattributed_human(),
        ]);
    let commit_b = repo_b
        .stage_all_and_commit("Harness Bash Python write")
        .unwrap();
    repo_b
        .filename("dsh-bash.txt")
        .assert_committed_lines(lines!["base bash".unattributed_human(), "AI bash".ai(),]);

    for commit in [commit_a, commit_b] {
        let sessions = &commit.authorship_log.metadata.sessions;
        assert_eq!(sessions.len(), 1);
        let agent = &sessions.values().next().unwrap().agent_id;
        assert_eq!(agent.tool, "deepseek-harness");
        assert_eq!(agent.model, "poc-model");
        assert_eq!(agent.id, SESSION_ID);
    }

    let stats_a = repo_a.stats().unwrap();
    assert_eq!(stats_a.ai_additions, 2);
    assert_eq!(stats_a.unknown_additions, 1);
    let stats_b = repo_b.stats().unwrap();
    assert_eq!(stats_b.ai_additions, 1);
    assert_eq!(stats_b.unknown_additions, 0);
}
