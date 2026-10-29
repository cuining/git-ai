use super::*;
use crate::repos::test_file::ExpectedLine;

fn rewrite_repo(database: &str) -> TestRepo {
    TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_METRICS_DB_PATH", database),
        ("GIT_AI_ENTERPRISE_METRICS", "true"),
        ("GIT_AI_REWRITE_METRICS_EVENTS", "false"),
    ])
}

fn head(repo: &TestRepo) -> String {
    repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned()
}

#[test]
fn export_metrics_unit2_amend_replaces_original_without_rewrite_upload_events() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    ai_write(&repo, "amended.rs", "let answer = 42;\nlet other = 7;\n");
    fs::write(
        repo.path().join("amended.rs"),
        "let answer = 43;\nlet other = 7;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "amended.rs"])
        .unwrap();
    let original = repo.stage_all_and_commit("original message").unwrap();
    repo.filename("amended.rs")
        .assert_committed_lines(crate::lines![
            "let answer = 43;".human(),
            "let other = 7;".ai(),
        ]);
    let before = exported_commit(&database, &original.commit_sha);
    assert_eq!(before["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(before["prod_ai_code_line_accept_num"], 1);

    repo.git(&["commit", "--amend", "-m", "replacement message"])
        .unwrap();
    repo.filename("amended.rs")
        .assert_committed_lines(crate::lines![
            "let answer = 43;".human(),
            "let other = 7;".ai(),
        ]);
    let amended = head(&repo);
    assert_ne!(amended, original.commit_sha);
    let record = exported_commit(&database, &amended);
    assert_eq!(
        record["source_commits"],
        json!([original.commit_sha.clone()])
    );
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(record["ai_breakdown"][0]["agent_type"], "codex");
    assert!(record["prod_ai_code_line_gen_num"].is_null());
    let snapshot = exported(&database, &[]);
    assert!(
        !snapshot["commits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["commit"] == original.commit_sha)
    );
    assert_eq!(snapshot["commits"], exported(&database, &[])["commits"]);
}

#[test]
fn export_metrics_unit2_rebase_replaces_only_the_replayed_commit() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    let base = repo.stage_all_and_commit("shared base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "topic"]).unwrap();
    ai_write(&repo, "rebased.rs", "let first = 1;\nlet second = 2;\n");
    let original = repo.stage_all_and_commit("topic source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("rebased.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai(), "let second = 2;".ai()]);
    assert_eq!(
        exported_commit(&database, &original.commit_sha)["prod_ai_surviving_source_line_num"],
        2
    );

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    let advance = repo.stage_all_and_commit("advance main parent").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("rebased.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["checkout", "topic"]).unwrap();
    repo.git(&["rebase", &main]).unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("rebased.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai(), "let second = 2;".ai()]);
    let rebased = head(&repo);
    assert_ne!(rebased, original.commit_sha);
    let record = exported_commit(&database, &rebased);
    assert_eq!(
        record["source_commits"],
        json!([original.commit_sha.clone()])
    );
    assert_eq!(
        record["parent_commits"],
        json!([advance.commit_sha.clone()])
    );
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 2);
    let snapshot = exported(&database, &[]);
    let commits = snapshot["commits"].as_array().unwrap();
    assert!(
        !commits
            .iter()
            .any(|record| record["commit"] == original.commit_sha)
    );
    for sha in [&base.commit_sha, &advance.commit_sha, &rebased] {
        assert_eq!(
            commits
                .iter()
                .filter(|record| record["commit"] == *sha)
                .count(),
            1
        );
    }
    assert_eq!(snapshot["commits"], exported(&database, &[])["commits"]);
}

#[test]
fn export_metrics_unit2_cherry_pick_and_revert_keep_their_sources() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("shared base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "source"]).unwrap();
    ai_write(&repo, "picked.rs", "let picked = 7;\n");
    let source = repo.stage_all_and_commit("source change").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("picked.rs")
        .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
    let source_record = exported_commit(&database, &source.commit_sha);
    assert_eq!(source_record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(source_record["prod_ai_code_line_accept_num"], 1);

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    let advance = repo
        .stage_all_and_commit("different cherry-pick parent")
        .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("picked.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["cherry-pick", &source.commit_sha]).unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("picked.rs")
        .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
    let picked = head(&repo);
    assert_ne!(picked, source.commit_sha);
    let record = exported_commit(&database, &picked);
    assert_eq!(record["source_commits"], json!([source.commit_sha.clone()]));
    assert_eq!(
        record["parent_commits"],
        json!([advance.commit_sha.clone()])
    );
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(
        exported_commit(&database, &source.commit_sha),
        source_record
    );

    repo.git(&["revert", "--no-edit", &picked]).unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("picked.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    let reverted = head(&repo);
    let record = exported_commit(&database, &reverted);
    assert_eq!(record["source_commits"], json!([picked.clone()]));
    assert_eq!(record["parent_commits"], json!([picked.clone()]));
    assert_eq!(record["prod_code_line_num"], 0);
    assert_eq!(record["prod_code_delete_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    for sha in [source.commit_sha.as_str(), picked.as_str()] {
        let retained = exported_commit(&database, sha);
        assert_eq!(retained["prod_ai_surviving_source_line_num"], 1);
        assert_eq!(retained["prod_ai_code_line_accept_num"], 1);
    }
    let snapshot = exported(&database, &[]);
    assert_eq!(snapshot["commits"], exported(&database, &[])["commits"]);
}

#[test]
fn export_metrics_unit2_parent_source_survives_rename_and_human_token_edit() {
    let (dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    let original = "let alpha_with_retained_context = 11;\nlet beta_with_retained_context = 21;\nlet gamma_with_retained_context = 31;\n";
    ai_write(&repo, "src/original.rs", original);
    let initial = repo.stage_all_and_commit("committed AI source").unwrap();
    repo.filename("src/original.rs")
        .assert_committed_lines(original.lines().map(|line| line.ai()).collect::<Vec<_>>());
    assert_eq!(
        exported_commit(&database, &initial.commit_sha)["prod_ai_surviving_source_line_num"],
        3
    );

    fs::create_dir_all(repo.path().join("tests")).unwrap();
    repo.git(&["mv", "src/original.rs", "tests/renamed.py"])
        .unwrap();
    fs::write(
        repo.path().join("tests/renamed.py"),
        original.replace("= 11;", "= 12;"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "tests/renamed.py"])
        .unwrap();
    let renamed = repo
        .stage_all_and_commit("rename and edit one token")
        .unwrap();
    repo.filename("src/original.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.filename("tests/renamed.py")
        .assert_committed_lines(crate::lines![
            "let alpha_with_retained_context = 12;".human(),
            "let beta_with_retained_context = 21;".ai(),
            "let gamma_with_retained_context = 31;".ai(),
        ]);
    let record = exported_commit(&database, &renamed.commit_sha);
    assert_eq!(record["prod_code_line_num"], 0);
    assert_eq!(record["prod_code_delete_line_num"], 1);
    assert_eq!(record["test_code_line_num"], 1);
    assert_eq!(record["test_code_delete_line_num"], 0);
    assert_eq!(record["test_ai_surviving_source_line_num"], 1);
    assert_eq!(record["test_ai_code_line_accept_num"], 0);
    assert_eq!(record["changed_file_num"], 1);
    assert_eq!(record["ai_breakdown"][0]["agent_type"], "codex");
    assert_eq!(
        record["ai_breakdown"][0]["test_ai_surviving_source_line_num"],
        1
    );
    let languages: Value =
        serde_json::from_str(record["language_stats"].as_str().unwrap()).unwrap();
    assert_eq!(languages["Rust"]["prod_code_delete_line_num"], 1);
    assert_eq!(languages["Python"]["test_code_line_num"], 1);

    let config = dir.path().join("rename-classification.json");
    fs::write(
        &config,
        r#"{"test_patterns":["src/**"],"language_extensions":{"rs":"InternalRust","py":"InternalPython"}}"#,
    )
    .unwrap();
    let snapshot = exported(&database, &["--config", config.to_str().unwrap()]);
    let record = snapshot["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["commit"] == renamed.commit_sha)
        .unwrap();
    assert_eq!(record["prod_code_line_num"], 1);
    assert_eq!(record["test_code_delete_line_num"], 1);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["changed_file_num"], 1);
    let languages: Value =
        serde_json::from_str(record["language_stats"].as_str().unwrap()).unwrap();
    assert_eq!(languages["InternalRust"]["test_code_delete_line_num"], 1);
    assert_eq!(languages["InternalPython"]["prod_code_line_num"], 1);
}

#[test]
fn export_metrics_unit2_clean_merges_require_more_than_half_of_all_related_lines() {
    for ai_count in [49_usize, 50, 51] {
        let (_dir, database) = isolated_metrics_db_path();
        let repo = rewrite_repo(&database);
        fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
        repo.stage_all_and_commit("shared base").unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        let main = repo.current_branch();
        repo.git(&["checkout", "-b", "side"]).unwrap();
        let ai_text: String = (0..ai_count)
            .map(|index| format!("let side_ai_{index} = {index};\n"))
            .collect();
        let edited_ai_text = ai_text.replacen(" = 0;", " = 1000;", 1);
        let human_text: String = (0..20)
            .map(|index| format!("let side_human_{index} = {index};\n"))
            .collect();
        let excluded_count = 100 - ai_count - 20;
        let excluded_text: String = (0..excluded_count)
            .map(|index| format!("let side_excluded_{index} = {index};\n"))
            .collect();
        ai_write(&repo, "imported.rs", &ai_text);
        fs::write(
            repo.path().join("imported.rs"),
            format!("{edited_ai_text}{human_text}"),
        )
        .unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", "imported.rs"])
            .unwrap();
        fs::write(repo.path().join("unrecognized.md"), &excluded_text).unwrap();
        let expected: Vec<_> = edited_ai_text
            .lines()
            .enumerate()
            .map(|(index, line)| if index == 0 { line.human() } else { line.ai() })
            .chain(human_text.lines().map(|line| line.human()))
            .collect();
        let side = repo
            .stage_all_and_commit("one hundred related additions")
            .unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        repo.filename("imported.rs")
            .assert_committed_lines(expected.clone());
        repo.filename("unrecognized.md").assert_committed_lines(
            excluded_text
                .lines()
                .map(|line| line.human())
                .collect::<Vec<_>>(),
        );
        let note = git_ai::authorship::authorship_log_serialization::AuthorshipLog::deserialize_from_string(&repo.git(&["notes", "--ref=ai", "show", &side.commit_sha]).unwrap()).unwrap();
        assert!(
            note.attestations
                .iter()
                .find(|file| file.file_path == "unrecognized.md")
                .unwrap()
                .entries
                .iter()
                .all(|entry| entry.hash.starts_with("h_"))
        );
        let side_record = exported_commit(&database, &side.commit_sha);
        assert_eq!(side_record["prod_code_line_num"], 100);
        assert_eq!(side_record["prod_ai_surviving_source_line_num"], ai_count);
        assert_eq!(side_record["prod_ai_code_line_accept_num"], ai_count - 1);
        assert_eq!(side_record["known_human_line_num"], 101 - ai_count);
        assert_eq!(side_record["unknown_line_num"], 0);
        assert_eq!(side_record["ai_breakdown"][0]["agent_type"], "codex");

        repo.git(&["checkout", &main]).unwrap();
        fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
        let advance = repo.stage_all_and_commit("advance first parent").unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        repo.filename("main.rs")
            .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
        repo.filename("imported.rs")
            .assert_committed_lines(Vec::<ExpectedLine>::new());
        repo.git(&["merge", "--no-ff", "side", "-m", "clean two-parent merge"])
            .unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        repo.filename("main.rs")
            .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
        repo.filename("imported.rs")
            .assert_committed_lines(expected);
        repo.filename("unrecognized.md").assert_committed_lines(
            excluded_text
                .lines()
                .map(|line| line.human())
                .collect::<Vec<_>>(),
        );
        let merge = head(&repo);
        let parents = repo.git(&["show", "-s", "--format=%P", "HEAD"]).unwrap();
        assert_eq!(
            parents.trim(),
            format!("{} {}", advance.commit_sha, side.commit_sha)
        );
        assert_eq!(exported_commit(&database, &side.commit_sha), side_record);
        let snapshot = exported(&database, &[]);
        let records: Vec<_> = snapshot["commits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["commit"] == merge)
            .collect();
        if ai_count == 51 {
            assert_eq!(
                records.len(),
                1,
                "a verified 51/100 clean merge must be exported"
            );
            let record = records[0];
            assert_eq!(
                record["parent_commits"],
                json!([advance.commit_sha, side.commit_sha])
            );
            assert_eq!(record["prod_code_line_num"], 100);
            assert_eq!(record["prod_ai_surviving_source_line_num"], 51);
            assert_eq!(record["prod_ai_code_line_accept_num"], 50);
            assert_eq!(record["known_human_line_num"], 50);
            assert_eq!(record["unknown_line_num"], 0);
            assert_eq!(record["changed_file_num"], 2);
            assert_eq!(record["ai_breakdown"][0]["agent_type"], "codex");
            assert!(record["prod_ai_code_line_gen_num"].is_null());
        } else {
            assert!(
                records.is_empty(),
                "{ai_count}/100 must not pass the strict merge threshold"
            );
        }
        assert_eq!(snapshot["commits"], exported(&database, &[])["commits"]);
    }
}

#[test]
fn export_metrics_unit2_amend_replays_erasure_before_retyping_and_new_ai() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    let text = "let retained_context = 7;\n";
    ai_write(&repo, "original.rs", text);
    let original = repo.stage_all_and_commit("original source").unwrap();
    repo.filename("original.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 7;".ai()]);
    fs::write(repo.path().join("original.rs"), "").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "original.rs"])
        .unwrap();
    fs::write(repo.path().join("original.rs"), text).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "original.rs"])
        .unwrap();
    ai_write(&repo, "tests/new.py", "new_ai = 1\n");
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "replacement with new source"])
        .unwrap();
    repo.filename("original.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 7;".human()]);
    repo.filename("tests/new.py")
        .assert_committed_lines(crate::lines!["new_ai = 1".ai()]);
    let replacement = head(&repo);
    let record = exported_commit(&database, &replacement);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["test_ai_surviving_source_line_num"], 1);
    assert_eq!(record["test_ai_code_line_accept_num"], 1);
    assert_eq!(record["source_commits"], json!([original.commit_sha]));
}

#[test]
fn export_metrics_unit2_parent_history_does_not_restore_erased_source() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    ai_write(&repo, "history.rs", "let original_context = 7;\n");
    repo.stage_all_and_commit("source").unwrap();
    repo.filename("history.rs")
        .assert_committed_lines(crate::lines!["let original_context = 7;".ai()]);
    fs::write(repo.path().join("unrelated.txt"), "unrelated\n").unwrap();
    repo.stage_all_and_commit("unrelated parent").unwrap();
    repo.filename("history.rs")
        .assert_committed_lines(crate::lines!["let original_context = 7;".ai()]);
    repo.filename("unrelated.txt")
        .assert_committed_lines(crate::lines!["unrelated".unattributed_human()]);
    fs::write(repo.path().join("history.rs"), "").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "history.rs"])
        .unwrap();
    fs::write(
        repo.path().join("history.rs"),
        "let original_context = 8;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "history.rs"])
        .unwrap();
    let edited = repo.stage_all_and_commit("human retyping").unwrap();
    repo.filename("history.rs")
        .assert_committed_lines(crate::lines!["let original_context = 8;".human()]);
    repo.filename("unrelated.txt")
        .assert_committed_lines(crate::lines!["unrelated".unattributed_human()]);
    let record = exported_commit(&database, &edited.commit_sha);
    assert_eq!(record["prod_code_line_num"], 1);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
}

#[test]
fn export_metrics_unit2_squash_merge_keeps_all_source_commits_as_provenance() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "first.rs", "let first = 1;\n");
    let first = repo.stage_all_and_commit("first source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai()]);
    ai_write(&repo, "second.rs", "let second = 2;\n");
    let second = repo.stage_all_and_commit("second source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai()]);
    repo.filename("second.rs")
        .assert_committed_lines(crate::lines!["let second = 2;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    repo.stage_all_and_commit("advance main").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.filename("second.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["merge", "--squash", "side"]).unwrap();
    let squash = repo.stage_all_and_commit("squashed sources").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai()]);
    repo.filename("second.rs")
        .assert_committed_lines(crate::lines!["let second = 2;".ai()]);
    let record = exported_commit(&database, &squash.commit_sha);
    let mut sources = vec![first.commit_sha.clone(), second.commit_sha.clone()];
    sources.sort();
    assert_eq!(record["source_commits"], json!(sources));
    assert!(record["source_commit"].is_null());
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 2);
    assert_eq!(
        exported_commit(&database, &first.commit_sha)["prod_ai_surviving_source_line_num"],
        1
    );
    assert_eq!(
        exported_commit(&database, &second.commit_sha)["prod_ai_surviving_source_line_num"],
        1
    );
}

#[test]
fn export_metrics_unit2_identical_import_with_repeated_lines_is_eligible() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(
        &repo,
        "repeated.rs",
        "let repeated = 1;\nlet repeated = 1;\n",
    );
    repo.stage_all_and_commit("repeated source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("repeated.rs")
        .assert_committed_lines(crate::lines![
            "let repeated = 1;".ai(),
            "let repeated = 1;".ai()
        ]);
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["merge", "--no-ff", "side", "-m", "repeated import"])
        .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("repeated.rs")
        .assert_committed_lines(crate::lines![
            "let repeated = 1;".ai(),
            "let repeated = 1;".ai()
        ]);
    let merge = head(&repo);
    let record = exported_commit(&database, &merge);
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 2);
    assert_eq!(record["merge_eligibility"]["related_additions"], 2);
    assert_eq!(record["merge_eligibility"]["verified_ai"], 2);
}

#[test]
fn export_metrics_unit2_review_later_squash_source_erasure_is_authoritative() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "source.rs", "let retained_context = 7;\n");
    let first = repo.stage_all_and_commit("AI source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 7;".ai()]);
    fs::write(repo.path().join("source.rs"), "").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    fs::write(repo.path().join("source.rs"), "let retained_context = 8;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    let second = repo
        .stage_all_and_commit("human erasure and retyping")
        .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 8;".human()]);
    assert_eq!(
        exported_commit(&database, &second.commit_sha)["prod_ai_surviving_source_line_num"],
        0
    );
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    repo.stage_all_and_commit("advance main").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["merge", "--squash", "side"]).unwrap();
    let squash = repo.stage_all_and_commit("squashed chronology").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 8;".human()]);
    let record = exported_commit(&database, &squash.commit_sha);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    let mut sources = vec![first.commit_sha, second.commit_sha];
    sources.sort();
    assert_eq!(record["source_commits"], json!(sources));
    assert!(record["source_commit"].is_null());
}

#[test]
fn export_metrics_unit2_review_cherry_pick_no_commit_retains_source_identity() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "source.rs", "let picked = 7;\n");
    let source = repo.stage_all_and_commit("AI source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    repo.stage_all_and_commit("advance main").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["cherry-pick", "--no-commit", &source.commit_sha])
        .unwrap();
    let picked = repo.stage_all_and_commit("deferred provenance").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
    let record = exported_commit(&database, &picked.commit_sha);
    assert_eq!(record["source_commits"], json!([source.commit_sha.clone()]));
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(
        exported_commit(&database, &source.commit_sha)["prod_ai_surviving_source_line_num"],
        1
    );
}

#[test]
fn export_metrics_unit2_review_deferred_cherry_pick_replays_live_source_history() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "source.rs", "let picked = 7;\n");
    let source = repo.stage_all_and_commit("AI source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    repo.stage_all_and_commit("advance main").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["cherry-pick", "--no-commit", &source.commit_sha])
        .unwrap();
    fs::write(repo.path().join("source.rs"), "").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    fs::write(repo.path().join("source.rs"), "let picked = 8;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    ai_write(&repo, "tests/local.py", "new_ai = 1\n");
    let picked = repo.stage_all_and_commit("deferred provenance").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let picked = 8;".human()]);
    repo.filename("tests/local.py")
        .assert_committed_lines(crate::lines!["new_ai = 1".ai()]);
    let record = exported_commit(&database, &picked.commit_sha);
    assert_eq!(record["source_commits"], json!([source.commit_sha.clone()]));
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["test_ai_surviving_source_line_num"], 1);
    assert_eq!(record["test_ai_code_line_accept_num"], 1);
    assert_eq!(
        exported_commit(&database, &source.commit_sha)["prod_ai_surviving_source_line_num"],
        1
    );
}

#[test]
fn export_metrics_unit2_review_rebase_preserves_later_erasure() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "source.rs", "let retained_context = 7;\n");
    let first = repo.stage_all_and_commit("AI source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 7;".ai()]);
    fs::write(repo.path().join("source.rs"), "").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    fs::write(repo.path().join("source.rs"), "let retained_context = 8;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    let second = repo
        .stage_all_and_commit("human erasure and retyping")
        .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 8;".human()]);
    assert_eq!(
        exported_commit(&database, &second.commit_sha)["prod_ai_surviving_source_line_num"],
        0
    );
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    repo.stage_all_and_commit("advance main").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.git(&["checkout", "side"]).unwrap();
    repo.git(&["rebase", &main]).unwrap();
    let rebased = head(&repo);
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("source.rs")
        .assert_committed_lines(crate::lines!["let retained_context = 8;".human()]);
    let record = exported_commit(&database, &rebased);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 0);
    assert_eq!(record["prod_ai_code_line_accept_num"], 0);
    assert_eq!(record["source_commits"], json!([second.commit_sha]));
    let snapshot = exported(&database, &[]);
    assert!(
        !snapshot["commits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["commit"] == first.commit_sha)
    );
}

#[test]
fn export_metrics_unit2_local_merge_stays_out_of_enabled_legacy_uploads() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_METRICS_DB_PATH", &database),
        ("GIT_AI_ENTERPRISE_METRICS", "true"),
        ("GIT_AI_REWRITE_METRICS_EVENTS", "true"),
    ]);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "imported.rs", "let imported = 1;\n");
    repo.stage_all_and_commit("AI side").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("imported.rs")
        .assert_committed_lines(crate::lines!["let imported = 1;".ai()]);
    repo.git(&["commit", "--amend", "-m", "legacy rewrite still enabled"])
        .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("imported.rs")
        .assert_committed_lines(crate::lines!["let imported = 1;".ai()]);
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count = || {
        connection
            .query_row(
                "SELECT count(*) FROM metrics WHERE event_kind = 7",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while count() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(count(), 1);
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["merge", "--no-ff", "side", "-m", "local merge"])
        .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("imported.rs")
        .assert_committed_lines(crate::lines!["let imported = 1;".ai()]);
    let record = exported_commit(&database, &head(&repo));
    assert_eq!(record["prod_ai_surviving_source_line_num"], 1);
    assert_eq!(record["prod_ai_code_line_accept_num"], 1);
    assert_eq!(count(), 1);
    assert_eq!(connection.query_row("SELECT count(*) FROM metrics WHERE event_json LIKE '%source_history%' OR event_json LIKE '%source_snapshot%' OR event_json LIKE '%relations%'", [], |row| row.get::<_, u64>(0)).unwrap(), 0);
}

#[test]
fn export_metrics_unit2_deferred_partial_source_survives_initial() {
    for amend in [false, true] {
        let (_dir, database) = isolated_metrics_db_path();
        let repo = rewrite_repo(&database);
        fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
        repo.stage_all_and_commit("base").unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        let main = repo.current_branch();
        repo.git(&["checkout", "-b", "side"]).unwrap();
        ai_write(&repo, "source.rs", "let picked = 7;\n");
        let source = repo.stage_all_and_commit("AI source").unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        repo.filename("source.rs")
            .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
        if !amend {
            repo.git(&["checkout", &main]).unwrap();
            repo.git(&["cherry-pick", "--no-commit", &source.commit_sha])
                .unwrap();
        }
        ai_write(&repo, "tests/local.py", "new_ai = 1\n");
        repo.git(&["add", "source.rs"]).unwrap();
        if amend {
            repo.git(&["commit", "--amend", "-m", "partial amend"])
                .unwrap();
        } else {
            repo.git(&["commit", "-m", "partial deferred pick"])
                .unwrap();
        }
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        repo.filename("source.rs")
            .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
        assert!(
            repo.git_og(&["cat-file", "-e", "HEAD:tests/local.py"])
                .is_err()
        );
        let first = exported_commit(&database, &head(&repo));
        assert_eq!(first["prod_ai_surviving_source_line_num"], 1);
        assert_eq!(first["test_ai_surviving_source_line_num"], 0);
        let second = repo.stage_all_and_commit("commit carried source").unwrap();
        repo.filename("anchor.rs")
            .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
        repo.filename("source.rs")
            .assert_committed_lines(crate::lines!["let picked = 7;".ai()]);
        repo.filename("tests/local.py")
            .assert_committed_lines(crate::lines!["new_ai = 1".ai()]);
        let record = exported_commit(&database, &second.commit_sha);
        assert_eq!(record["prod_code_line_num"], 0);
        assert_eq!(record["test_code_line_num"], 1);
        assert_eq!(record["test_ai_surviving_source_line_num"], 1);
        assert_eq!(record["test_ai_code_line_accept_num"], 1);
    }
}

#[test]
fn export_metrics_unit2_source_only_observations_do_not_add_legacy_checkpoints() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let base = head(&repo);
    fs::write(repo.path().join("empty.rs"), "").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "empty.rs"])
        .unwrap();
    let log = repo.working_logs_for_base_commit(&base);
    let checkpoints = log.read_all_checkpoints().unwrap();
    let observation = checkpoints
        .iter()
        .flat_map(|checkpoint| &checkpoint.entries)
        .find(|entry| entry.file == "empty.rs")
        .unwrap();
    assert!(observation.source_observation_only);
    assert!(observation.attributions.is_empty());
    assert!(observation.line_attributions.is_empty());
    assert!(observation.ai_source.as_ref().unwrap().matches(""));
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count = || {
        connection.query_row("SELECT count(*) FROM metrics WHERE event_kind = 4 AND json_extract(event_json, '$.v.\"1\"') = 'known_human'", [], |row| row.get::<_, u64>(0)).unwrap()
    };
    assert_eq!(count(), 0);
    ai_write(&repo, "source.rs", "let unchanged = 7;\n");
    fs::write(repo.path().join("source.rs"), "let unchanged = 7;\r\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "source.rs"])
        .unwrap();
    repo.working_logs_for_base_commit(&base);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while count() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(count(), 1);
    assert_eq!(connection.query_row("SELECT count(*) FROM metrics WHERE event_kind = 4 AND json_extract(event_json, '$.v.\"1\"') = 'known_human' AND json_extract(event_json, '$.v.\"3\"') = 0 AND json_extract(event_json, '$.v.\"4\"') = 0", [], |row| row.get::<_, u64>(0)).unwrap(), 1);
}

#[test]
fn export_metrics_unit2_conflict_resolution_is_withheld_with_a_reason() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("conflict.rs"), "let value = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("conflict.rs")
        .assert_committed_lines(crate::lines!["let value = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "conflict.rs", "let value = 1;\n");
    repo.stage_all_and_commit("side AI").unwrap();
    repo.filename("conflict.rs")
        .assert_committed_lines(crate::lines!["let value = 1;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("conflict.rs"), "let value = 2;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict.rs"])
        .unwrap();
    repo.stage_all_and_commit("main human").unwrap();
    repo.filename("conflict.rs")
        .assert_committed_lines(crate::lines!["let value = 2;".human()]);
    assert!(repo.git(&["merge", "--no-ff", "side"]).is_err());
    fs::write(repo.path().join("conflict.rs"), "let value = 3;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "conflict.rs"])
        .unwrap();
    let resolved = repo.stage_all_and_commit("manual resolution").unwrap();
    repo.filename("conflict.rs")
        .assert_committed_lines(crate::lines!["let value = 3;".human()]);
    let snapshot = exported(&database, &[]);
    assert!(
        !snapshot["commits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["commit"] == resolved.commit_sha)
    );
    assert!(
        snapshot["coverage"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["reason"] == "merge_conflict_or_manual_resolution")
    );
}

#[test]
fn export_metrics_unit2_octopus_merge_is_withheld_with_a_reason() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side-one"]).unwrap();
    ai_write(&repo, "one.rs", "let one = 1;\n");
    repo.stage_all_and_commit("first side").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("one.rs")
        .assert_committed_lines(crate::lines!["let one = 1;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["checkout", "-b", "side-two"]).unwrap();
    ai_write(&repo, "two.rs", "let two = 2;\n");
    repo.stage_all_and_commit("second side").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("one.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.filename("two.rs")
        .assert_committed_lines(crate::lines!["let two = 2;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    repo.git(&[
        "merge",
        "--no-ff",
        "side-one",
        "side-two",
        "-m",
        "octopus merge",
    ])
    .unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("one.rs")
        .assert_committed_lines(crate::lines!["let one = 1;".ai()]);
    repo.filename("two.rs")
        .assert_committed_lines(crate::lines!["let two = 2;".ai()]);
    let merge = head(&repo);
    let snapshot = exported(&database, &[]);
    assert!(
        !snapshot["commits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["commit"] == merge)
    );
    assert!(
        snapshot["coverage"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["reason"] == "merge_octopus")
    );
}

#[test]
fn export_metrics_unit2_rebase_autosquash_preserves_all_replacement_sources() {
    let (_dir, database) = isolated_metrics_db_path();
    let repo = rewrite_repo(&database);
    fs::write(repo.path().join("anchor.rs"), "let anchor = 0;\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "side"]).unwrap();
    ai_write(&repo, "first.rs", "let first = 1;\n");
    let first = repo.stage_all_and_commit("first source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai()]);
    ai_write(&repo, "second.rs", "let second = 2;\n");
    let second = repo.stage_all_and_commit("fixup! first source").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai()]);
    repo.filename("second.rs")
        .assert_committed_lines(crate::lines!["let second = 2;".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.rs"), "let main = 3;\n").unwrap();
    repo.stage_all_and_commit("advance main").unwrap();
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    repo.filename("second.rs")
        .assert_committed_lines(Vec::<ExpectedLine>::new());
    let onto = head(&repo);
    repo.git(&["checkout", "side"]).unwrap();
    repo.git_with_env(
        &["rebase", "-i", "--autosquash", &main],
        &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();
    let squashed = head(&repo);
    repo.filename("anchor.rs")
        .assert_committed_lines(crate::lines!["let anchor = 0;".unattributed_human()]);
    repo.filename("main.rs")
        .assert_committed_lines(crate::lines!["let main = 3;".unattributed_human()]);
    repo.filename("first.rs")
        .assert_committed_lines(crate::lines!["let first = 1;".ai()]);
    repo.filename("second.rs")
        .assert_committed_lines(crate::lines!["let second = 2;".ai()]);
    let record = exported_commit(&database, &squashed);
    let mut sources = vec![first.commit_sha.clone(), second.commit_sha.clone()];
    sources.sort();
    assert_eq!(record["source_commits"], json!(sources));
    assert!(record["source_commit"].is_null());
    assert_eq!(record["prod_code_line_num"], 2);
    assert_eq!(record["prod_ai_surviving_source_line_num"], 2);
    assert_eq!(record["prod_ai_code_line_accept_num"], 2);
    assert_eq!(record["parent_commits"], json!([onto]));
    let snapshot = exported(&database, &[]);
    assert!(
        !snapshot["commits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["commit"] == first.commit_sha || row["commit"] == second.commit_sha)
    );
}
