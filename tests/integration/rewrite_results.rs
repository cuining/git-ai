use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{
    codex_checkpoint, extract_json_object, isolated_metrics_db_path, sparse_str,
};
use git_ai::git::repository::parse_git_version;
use git_ai::metrics::attrs::attr_pos;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::events::rewrite_committed_pos;
use git_ai::metrics::types::MetricEventId;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const REWRITE_RESULT_POSITION: usize = 17;

fn rewrite_repo() -> (tempfile::TempDir, String, TestRepo) {
    let (metrics_dir, metrics_path) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_path.as_str()),
        ("GIT_AI_REWRITE_METRICS_EVENTS", "true"),
        ("GIT_AI_UNTRACED_COMMIT_FIXUP", "false"),
    ]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/company/rewrite.git",
    ])
    .unwrap();
    (metrics_dir, metrics_path, repo)
}

fn head(repo: &TestRepo, revision: &str) -> String {
    repo.git(&["rev-parse", revision])
        .unwrap()
        .trim()
        .to_owned()
}

fn commit_ai_line(repo: &TestRepo, filename: &str, content: &str) -> String {
    let path = repo.path().join(filename);
    codex_checkpoint(
        repo,
        &path,
        "rewrite-result-session",
        "PreToolUse",
        filename,
    );
    fs::write(&path, format!("{content}\n")).unwrap();
    codex_checkpoint(
        repo,
        &path,
        "rewrite-result-session",
        "PostToolUse",
        filename,
    );
    let commit = repo.stage_all_and_commit(filename).unwrap();
    repo.filename(filename)
        .assert_committed_lines(lines![content.ai()]);
    commit.commit_sha
}

fn rewrite_result(db_path: &str, after_sha: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let db = MetricsDatabase::open_at_path(Path::new(db_path)).unwrap();
        let events = db
            .get_metric_history(0, None, &[MetricEventId::RewriteCommitted as u16])
            .unwrap();
        if let Some(result) = events.iter().find_map(|record| {
            let encoded = sparse_str(&record.event.values, REWRITE_RESULT_POSITION)?;
            let result: Value = serde_json::from_str(encoded).expect("valid ID 7 rewrite result");
            if result["after_sha"] != after_sha {
                return None;
            }
            assert_eq!(
                sparse_str(&record.event.attrs, attr_pos::REPO_URL),
                Some("https://example.invalid/company/rewrite")
            );
            Some(result)
        }) {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "missing ID 7 operation result for {after_sha}; saw {} rewrite metric(s)",
            events.len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Feature {
    trunk: String,
    base: String,
    first: String,
    tip: String,
}

fn prepare_feature(repo: &TestRepo) -> Feature {
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(lines!["base line".human()]);
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    base_file.assert_committed_lines(lines!["base line".human()]);
    let trunk = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let first = commit_ai_line(repo, "first.txt", "AI generated first");
    let tip = commit_ai_line(repo, "second.txt", "AI generated second");
    Feature {
        trunk,
        base,
        first,
        tip,
    }
}

fn rebase_feature(repo: &TestRepo, trunk: &str) -> (String, String) {
    repo.git(&["checkout", trunk]).unwrap();
    let mut trunk_file = repo.filename("trunk.txt");
    trunk_file.set_contents(lines!["advance trunk".human()]);
    repo.stage_all_and_commit("advance trunk").unwrap();
    trunk_file.assert_committed_lines(lines!["advance trunk".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", trunk]).unwrap();
    (head(repo, "HEAD^"), head(repo, "HEAD"))
}

#[test]
fn id7_rebase_reports_one_complete_scoped_multi_commit_result() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let Feature {
        trunk,
        first: old_first,
        tip: old_tip,
        ..
    } = prepare_feature(&repo);
    let (new_first, new_tip) = rebase_feature(&repo, &trunk);
    assert_ne!(old_first, new_first);
    assert_ne!(old_tip, new_tip);
    repo.filename("first.txt")
        .assert_committed_lines(lines!["AI generated first".ai()]);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);

    let result = rewrite_result(&metrics_path, &new_tip);
    let db = MetricsDatabase::open_at_path(Path::new(&metrics_path)).unwrap();
    let events = db
        .get_metric_history(0, None, &[MetricEventId::RewriteCommitted as u16])
        .unwrap();
    for target in [&new_first, &new_tip] {
        let event = events
            .iter()
            .find(|record| {
                sparse_str(&record.event.attrs, attr_pos::COMMIT_SHA) == Some(target.as_str())
            })
            .expect("each rewritten commit keeps its existing telemetry");
        assert_eq!(
            sparse_str(&event.event.attrs, attr_pos::BRANCH),
            Some("feature")
        );

        assert!(
            event
                .event
                .values
                .contains_key(&rewrite_committed_pos::AI_ACCEPTED.to_string())
        );
        assert!(
            event
                .event
                .values
                .contains_key(&rewrite_committed_pos::TOOL_MODEL_PAIRS.to_string())
        );
        assert!(sparse_str(&event.event.values, rewrite_committed_pos::AUTHORSHIP_NOTE).is_some());
    }
    assert_eq!(result["v"], 1);
    assert_eq!(result["kind"], "rebase");
    assert_eq!(result["ref"], "refs/heads/feature");
    assert_eq!(result["before_sha"], old_tip);
    assert_eq!(result["after_sha"], new_tip);
    assert_eq!(result["complete"], true);
    let mut expected_replaced = vec![old_first.as_str(), old_tip.as_str()];
    expected_replaced.sort_unstable();
    assert_eq!(sorted_shas(&result["replaced_shas"]), expected_replaced);

    let targets = result["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    for (target_sha, source_sha) in [(&new_first, &old_first), (&new_tip, &old_tip)] {
        let target = targets
            .iter()
            .find(|target| target["sha"] == *target_sha)
            .expect("each rewritten commit must have a target entry");
        assert_eq!(target["sources"], json!([source_sha]));
        assert_eq!(
            target["stats"],
            json!({"added_lines": 1, "deleted_lines": 0, "ai_lines": 1, "human_lines": 0})
        );
    }
}

#[test]
fn id7_squash_reports_all_sources_without_replacing_feature_branch() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let Feature {
        trunk,
        base,
        first,
        tip,
    } = prepare_feature(&repo);

    repo.git(&["checkout", &trunk]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    let squashed = repo.commit("squash feature").unwrap().commit_sha;
    repo.filename("first.txt")
        .assert_committed_lines(lines!["AI generated first".ai()]);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);
    assert_eq!(head(&repo, "refs/heads/feature"), tip);
    assert_eq!(head(&repo, "HEAD^{tree}"), head(&repo, "feature^{tree}"));

    let result = rewrite_result(&metrics_path, &squashed);
    assert_eq!(result["v"], 1);
    assert_eq!(result["kind"], "squash_merge");
    assert_eq!(result["ref"], format!("refs/heads/{trunk}"));
    assert_eq!(result["before_sha"], base);
    assert_eq!(result["after_sha"], squashed);
    assert_eq!(result["complete"], true);
    assert_eq!(result["replaced_shas"], json!([]));
    let targets = result["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["sha"], squashed);
    let mut expected_sources = vec![first.as_str(), tip.as_str()];
    expected_sources.sort_unstable();
    assert_eq!(sorted_shas(&targets[0]["sources"]), expected_sources);
    assert_eq!(
        targets[0]["stats"],
        json!({"added_lines": 2, "deleted_lines": 0, "ai_lines": 2, "human_lines": 0})
    );
}

#[rstest::rstest]
#[case::clean(false, true)]
#[case::with_extra_edit(true, false)]
fn id7_diverged_squash_requires_the_expected_merge_tree(
    #[case] extra_edit: bool,
    #[case] complete: bool,
) {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let Feature {
        trunk, first, tip, ..
    } = prepare_feature(&repo);
    repo.git(&["checkout", &trunk]).unwrap();
    let mut destination = repo.filename("destination.txt");
    destination.set_contents(lines!["independent destination line".human()]);
    let before = repo
        .stage_all_and_commit("advance destination")
        .unwrap()
        .commit_sha;
    destination.assert_committed_lines(lines!["independent destination line".human()]);

    repo.git(&["merge", "--squash", "feature"]).unwrap();
    if extra_edit {
        fs::write(repo.path().join("extra.txt"), "not part of the squash\n").unwrap();
    }
    let squashed = repo
        .stage_all_and_commit("squash feature")
        .unwrap()
        .commit_sha;
    if extra_edit {
        repo.filename("extra.txt")
            .assert_committed_lines(lines!["not part of the squash".unattributed_human()]);
    }
    destination.assert_committed_lines(lines!["independent destination line".human()]);
    repo.filename("first.txt")
        .assert_committed_lines(lines!["AI generated first".ai()]);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);
    assert_eq!(head(&repo, "refs/heads/feature"), tip);
    assert_ne!(head(&repo, "HEAD^{tree}"), head(&repo, "feature^{tree}"));

    let result = rewrite_result(&metrics_path, &squashed);
    assert_eq!(result["before_sha"], before);
    assert_eq!(result["complete"], complete);
    assert_eq!(result["replaced_shas"], json!([]));
    let mut sources = vec![first.as_str(), tip.as_str()];
    sources.sort_unstable();
    assert_eq!(sorted_shas(&result["targets"][0]["sources"]), sources);
    assert_eq!(result["targets"][0]["stats"]["ai_lines"], 2);
}

#[test]
fn id7_diverged_squash_uses_committed_union_attributes() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let version = repo.git(&["--version"]).unwrap();
    if parse_git_version(&version).is_some_and(|version| version < (2, 42, 0)) {
        return;
    }

    let mut attributes = repo.filename(".gitattributes");
    let mut changelog = repo.filename("changelog.txt");
    attributes.set_contents(lines!["changelog.txt merge=union".human()]);
    changelog.set_contents(lines!["original changelog entry".human()]);
    repo.stage_all_and_commit("base with union driver").unwrap();
    attributes.assert_committed_lines(lines!["changelog.txt merge=union".human()]);
    changelog.assert_committed_lines(lines!["original changelog entry".human()]);

    let trunk = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let source = commit_ai_line(&repo, "changelog.txt", "AI feature changelog entry");
    repo.git(&["checkout", &trunk]).unwrap();
    changelog.set_contents(lines!["human destination changelog entry".human()]);
    let before = repo
        .stage_all_and_commit("update destination changelog")
        .unwrap()
        .commit_sha;
    changelog.assert_committed_lines(lines!["human destination changelog entry".human()]);

    repo.git(&["merge", "--squash", "feature"]).unwrap();
    let squashed = repo.commit("squash with union driver").unwrap().commit_sha;
    changelog.assert_committed_lines(lines![
        "human destination changelog entry".human(),
        "AI feature changelog entry".ai(),
    ]);
    assert_eq!(head(&repo, "refs/heads/feature"), source);
    assert_ne!(head(&repo, "HEAD^{tree}"), head(&repo, "feature^{tree}"));

    let result = rewrite_result(&metrics_path, &squashed);
    assert_eq!(result["before_sha"], before);
    assert_eq!(result["complete"], true);
    assert_eq!(result["replaced_shas"], json!([]));
    assert_eq!(result["targets"][0]["sources"], json!([source]));
}

#[test]
fn id7_rebase_reports_replacement_when_a_targets_note_is_missing() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let Feature {
        trunk,
        first: old_first,
        tip: old_tip,
        ..
    } = prepare_feature(&repo);
    repo.git(&["config", "protocol.https.allow", "never"])
        .unwrap();
    repo.git(&["notes", "--ref=ai", "remove", &old_first])
        .unwrap();

    let (new_first, new_tip) = rebase_feature(&repo, &trunk);
    repo.filename("first.txt")
        .assert_committed_lines(lines!["AI generated first".unattributed_human()]);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);

    let result = rewrite_result(&metrics_path, &new_tip);
    assert_eq!(result["complete"], true);
    let mut expected_replaced = vec![old_first.as_str(), old_tip.as_str()];
    expected_replaced.sort_unstable();
    assert_eq!(sorted_shas(&result["replaced_shas"]), expected_replaced);
    let targets = result["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    let missing_note = targets
        .iter()
        .find(|target| target["sha"] == new_first)
        .expect("target without a note must remain in the operation result");
    assert_eq!(
        missing_note,
        &json!({"sha": new_first, "sources": [old_first], "stats": null})
    );
}

fn sorted_shas(value: &Value) -> Vec<&str> {
    let mut shas: Vec<_> = value
        .as_array()
        .unwrap()
        .iter()
        .map(|sha| sha.as_str().unwrap())
        .collect();
    shas.sort_unstable();
    shas
}

#[test]
fn id7_detached_amend_preserves_legacy_stats_without_replacing_a_branch_commit() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let Feature { tip: old_tip, .. } = prepare_feature(&repo);

    repo.git(&["checkout", "--detach", &old_tip]).unwrap();
    repo.git(&["commit", "--amend", "-m", "amended while detached"])
        .unwrap();
    let new_tip = head(&repo, "HEAD");
    assert_ne!(new_tip, old_tip);
    assert_eq!(head(&repo, "refs/heads/feature"), old_tip);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let db = MetricsDatabase::open_at_path(Path::new(&metrics_path)).unwrap();
        let records = db
            .get_metric_history(0, None, &[MetricEventId::RewriteCommitted as u16])
            .unwrap();
        if let Some(record) = records.iter().find(|record| {
            sparse_str(&record.event.attrs, attr_pos::COMMIT_SHA) == Some(new_tip.as_str())
        }) {
            use git_ai::metrics::events::rewrite_committed_pos as pos;
            let values = &record.event.values;
            assert_eq!(sparse_str(values, pos::OPERATION_KIND), Some("amend"));
            assert_eq!(
                values[&pos::ORIGINAL_COMMIT_SHAS.to_string()],
                json!([old_tip])
            );
            assert_eq!(values[&pos::GIT_DIFF_ADDED_LINES.to_string()], 1);
            assert_eq!(values[&pos::AI_ACCEPTED.to_string()][0], 1);
            assert!(sparse_str(values, REWRITE_RESULT_POSITION).is_none());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "missing legacy ID 7 stats for detached amend {new_tip}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn machine_usage_totals(repo: &TestRepo, metrics_path: &str, expected_commits: usize) -> Value {
    let parsed = machine_usage(repo, metrics_path, expected_commits);
    assert_eq!(parsed["pending_events"], 0);
    parsed["totals"].clone()
}

fn machine_usage(repo: &TestRepo, metrics_path: &str, expected_commits: usize) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let db = MetricsDatabase::open_at_path(Path::new(metrics_path)).unwrap();
        let commits = db
            .get_metric_history(0, None, &[MetricEventId::Committed as u16])
            .unwrap();
        if commits.len() >= expected_commits {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected_commits} persisted ID 1 metrics, got {}",
            commits.len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let today = chrono::Utc::now().date_naive();
    let start = today.pred_opt().unwrap().to_string();
    let end = today.succ_opt().unwrap().to_string();
    let output = repo
        .git_ai_with_env(
            &[
                "usage",
                "--start-date",
                &start,
                "--end-date",
                &end,
                "--json",
            ],
            &[("GIT_AI_TEST_METRICS_DB_PATH", metrics_path), ("TZ", "UTC")],
        )
        .expect("dated usage should read real locally generated ID 1/7 metrics");
    let parsed: Value = serde_json::from_str(&extract_json_object(&output)).unwrap();
    assert!(!parsed.as_object().unwrap().contains_key("user_id"));
    parsed
}

#[test]
fn usage_machine_real_rebase_keeps_original_contributions() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let feature = prepare_feature(&repo);
    let (_, new_tip) = rebase_feature(&repo, &feature.trunk);
    repo.filename("first.txt")
        .assert_committed_lines(lines!["AI generated first".ai()]);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);
    assert_eq!(rewrite_result(&metrics_path, &new_tip)["complete"], true);

    assert_eq!(
        machine_usage_totals(&repo, &metrics_path, 4),
        json!({
            "commit_count": 4, "added_lines": 4, "ai_lines": 2, "human_lines": 2, "unknown_lines": 0,
        })
    );
}

#[test]
fn usage_machine_real_squash_preserves_both_original_contributions() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let feature = prepare_feature(&repo);
    repo.git(&["checkout", &feature.trunk]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    let squashed = repo.commit("squash feature").unwrap().commit_sha;
    repo.filename("first.txt")
        .assert_committed_lines(lines!["AI generated first".ai()]);
    repo.filename("second.txt")
        .assert_committed_lines(lines!["AI generated second".ai()]);
    assert_eq!(head(&repo, "refs/heads/feature"), feature.tip);
    assert_eq!(rewrite_result(&metrics_path, &squashed)["complete"], true);

    assert_eq!(
        machine_usage_totals(&repo, &metrics_path, 3),
        json!({
            "commit_count": 3, "added_lines": 3, "ai_lines": 2, "human_lines": 1, "unknown_lines": 0,
        })
    );
}

#[test]
fn usage_machine_real_amend_resolves_to_its_original_contribution() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let original = commit_ai_line(&repo, "amended.txt", "AI generated original");
    repo.git(&["commit", "--amend", "-m", "new subject"])
        .unwrap();
    let amended = head(&repo, "HEAD");
    assert_ne!(amended, original);
    repo.filename("amended.txt")
        .assert_committed_lines(lines!["AI generated original".ai()]);

    let result = rewrite_result(&metrics_path, &amended);
    assert_eq!(result["complete"], true);
    assert_eq!(result["kind"], "amend");
    assert_eq!(
        machine_usage_totals(&repo, &metrics_path, 1),
        json!({"commit_count": 1, "added_lines": 1, "ai_lines": 1, "human_lines": 0, "unknown_lines": 0})
    );
}

#[test]
fn usage_machine_real_content_amend_keeps_new_work_pending() {
    let (_metrics_dir, metrics_path, repo) = rewrite_repo();
    let original = commit_ai_line(&repo, "amended.txt", "AI generated original");
    fs::write(
        repo.path().join("amended.txt"),
        "AI generated original\nAnother AI contribution\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "amended.txt"])
        .unwrap();
    repo.git(&["add", "amended.txt"]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    let amended = head(&repo, "HEAD");
    assert_ne!(amended, original);
    repo.filename("amended.txt").assert_committed_lines(lines![
        "AI generated original".ai(),
        "Another AI contribution".ai(),
    ]);

    assert_eq!(rewrite_result(&metrics_path, &amended)["complete"], true);
    let usage = machine_usage(&repo, &metrics_path, 1);
    assert_eq!(usage["totals"]["ai_lines"], 1);
    assert_eq!(usage["pending_events"], 1);
}
