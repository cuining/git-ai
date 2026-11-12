use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{extract_json_object, isolated_metrics_db_path};
use chrono::{Days, NaiveDate, Utc};
use git_ai::metrics::PosEncoded;
use git_ai::metrics::attrs::{EventAttributes, attr_pos};
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::events::CommittedValues;
use git_ai::metrics::types::MetricEvent;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

fn seed_ai_commit(repo: &TestRepo) {
    let mut file = repo.filename("app.rs");
    file.set_contents(crate::lines!["fn main() {}", "let answer = 42;".ai()]);
    repo.stage_all_and_commit("AI commit")
        .expect("AI commit should succeed");
    file.assert_committed_lines(crate::lines![
        "fn main() {}".human(),
        "let answer = 42;".ai()
    ]);
}

/// Run `git-ai usage` with the metrics DB pointed at the daemon's isolated path,
/// retrying until the just-committed activity is persisted or the deadline passes.
fn usage_json(repo: &TestRepo, metrics_db_path: &str, extra_args: &[&str]) -> Value {
    let mut args = vec!["usage"];
    args.extend_from_slice(extra_args);
    args.push("--json");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        repo.sync_daemon_force();
        let result =
            repo.git_ai_with_env(&args, &[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path)]);
        if let Ok(output) = &result {
            let json = extract_json_object(output);
            if let Ok(value) = serde_json::from_str::<Value>(&json) {
                return value;
            }
        }

        if Instant::now() >= deadline {
            panic!("usage {args:?} did not return activity data: {result:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The window spans `calendar_start` to `calendar_end` as local dates. A fixed
/// `days * 86400` second subtraction lands on the same wall-clock time N days back,
/// so the span is N days except within an hour of midnight on a DST-transition day,
/// where it can shift by one. Callers assert a `[N - 1, N]` range to tolerate that.
fn window_span_days(value: &Value) -> i64 {
    let parse = |key: &str| {
        let text = value[key].as_str().expect("date field should be a string");
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date field should parse")
    };
    (parse("calendar_end") - parse("calendar_start")).num_days()
}

fn assert_window(value: &Value, expected_label: &str, expected_days: i64) {
    assert_eq!(value["period_label"].as_str().unwrap(), expected_label);
    let span = window_span_days(value);
    assert!(
        (expected_days - 1..=expected_days).contains(&span),
        "expected a {expected_days}-day window (tolerating one DST day), got span {span}"
    );
}

#[test]
fn usage_period_valid_tokens_report_their_label_and_window() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);
    let cases = [
        ("1d", "last 24 hours", 1),
        ("3d", "last 3 days", 3),
        ("7d", "last 7 days", 7),
        ("30d", "last 30 days", 30),
    ];

    for (token, expected_label, expected_days) in cases {
        let value = usage_json(&repo, &metrics_db_path, &["--period", token]);
        assert_window(&value, expected_label, expected_days);
    }
}

#[test]
fn usage_period_accepts_the_equals_form() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    let value = usage_json(&repo, &metrics_db_path, &["--period=3d"]);

    assert_window(&value, "last 3 days", 3);
}

#[test]
fn usage_without_period_defaults_to_thirty_day_window() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    let value = usage_json(&repo, &metrics_db_path, &[]);

    assert_window(&value, "last 30 days", 30);
}

#[test]
fn usage_period_invalid_token_exits_nonzero_with_message() {
    let repo = TestRepo::new();

    let result = repo.git_ai(&["usage", "--period", "90d"]);

    let err = result.expect_err("an invalid --period value should exit non-zero");
    assert!(
        err.contains("Invalid --period value: 90d. Expected one of 1d, 3d, 7d, 30d."),
        "unexpected error output: {err}"
    );
}

#[test]
fn usage_period_missing_value_exits_nonzero_with_message() {
    let repo = TestRepo::new();

    let result = repo.git_ai(&["usage", "--period"]);

    let err = result.expect_err("a missing --period value should exit non-zero");
    assert!(
        err.contains("Missing value for --period."),
        "unexpected error output: {err}"
    );
}

fn usage_date_json(
    repo: &TestRepo,
    metrics_db_path: &str,
    start: NaiveDate,
    end: NaiveDate,
    repo_host: Option<&str>,
) -> Value {
    let start = start.to_string();
    let end = end.to_string();
    let mut args = vec![
        "usage",
        "--start-date",
        &start,
        "--end-date",
        &end,
        "--json",
    ];
    if let Some(host) = repo_host {
        args.extend(["--repo-host", host]);
    }
    let output = repo
        .git_ai_with_env(
            &args,
            &[
                ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path),
                ("TZ", "UTC"),
            ],
        )
        .expect("personal usage should be available from the local metrics DB");
    serde_json::from_str(&extract_json_object(&output)).expect("valid personal usage JSON")
}

fn personal_usage_date() -> NaiveDate {
    Utc::now()
        .date_naive()
        .checked_sub_days(Days::new(3))
        .unwrap()
}

fn event_timestamp(date: NaiveDate) -> u32 {
    date.and_hms_opt(9, 0, 0).unwrap().and_utc().timestamp() as u32
}

fn usage_commit_row(
    date: NaiveDate,
    repo_url: &str,
    sha: &str,
    person: Option<&str>,
    lines: [u32; 3],
) -> String {
    let [added, ai, human] = lines;
    let attrs = EventAttributes::with_version("test")
        .repo_url(repo_url)
        .branch("main")
        .commit_sha(sha);
    let attrs = match person {
        Some(person) => attrs.custom_attributes(json!({"dcc_user_id":person}).to_string()),
        None => attrs,
    };
    let values = CommittedValues::new()
        .git_diff_added_lines(added)
        .human_additions(human)
        .tool_model_pairs(vec!["all".to_string()])
        .ai_additions(vec![ai])
        .ai_accepted(vec![ai]);
    let event = MetricEvent::with_timestamp(event_timestamp(date), &values, attrs.to_sparse());
    serde_json::to_string(&event).unwrap()
}

fn usage_rewrite_row(date: NaiveDate, repo_url: &str, scope: Value) -> String {
    json!({
        "e": 7, "t": event_timestamp(date),
        "a": {"1": repo_url, "3": scope["after_sha"], "5": "main"},
        "v": {"17": scope.to_string()},
    })
    .to_string()
}

fn insert_personal_rows(path: &str, rows: &[String]) {
    MetricsDatabase::open_at_path(Path::new(path))
        .unwrap()
        .insert_events(rows)
        .unwrap();
}

#[test]
fn usage_machine_reports_local_id1_lines_and_the_inclusive_day() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new();
    let date = personal_usage_date();
    insert_personal_rows(
        &metrics_db_path,
        &[usage_commit_row(
            date,
            "https://example.invalid/company/usage",
            &"1".repeat(40),
            None,
            [5, 2, 1],
        )],
    );

    let actual = usage_date_json(&repo, &metrics_db_path, date, date, None);
    assert_eq!(
        actual,
        json!({
            "totals": {"commit_count": 1, "added_lines": 5, "ai_lines": 2, "human_lines": 1, "unknown_lines": 2},
            "daily": [{"date": date.to_string(), "commit_count": 1, "added_lines": 5, "ai_lines": 2, "human_lines": 1, "unknown_lines": 2}],
            "repos": [{"repo": "https://example.invalid/company/usage", "commit_count": 1, "added_lines": 5, "ai_lines": 2, "human_lines": 1, "unknown_lines": 2}],
            "pending_events": 0,
        })
    );
}

#[test]
fn usage_machine_filters_totals_and_daily_by_exact_repo_host() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new();
    let first = personal_usage_date();
    let second = first.succ_opt().unwrap();
    let internal = "git@git.xiaojukeji.com:team/first.git";
    let other_internal = "https://git.xiaojukeji.com:443/team/second.git";
    let external = "https://github.com/team/first";
    let similar_host = "https://git.xiaojukeji.com.example.invalid/team/second";
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let mut local: Value =
        serde_json::from_str(&usage_commit_row(first, "", &b, None, [19, 2, 0])).unwrap();
    let attrs = local["a"].as_object_mut().unwrap();
    attrs.remove(&attr_pos::REPO_URL.to_string());
    attrs.insert(attr_pos::LOCAL_REPO_ID.to_string(), json!("local-repo"));
    insert_personal_rows(
        &metrics_db_path,
        &[
            usage_commit_row(first, internal, &a, None, [5, 2, 1]),
            usage_commit_row(second, other_internal, &b, None, [7, 4, 1]),
            usage_commit_row(first, external, &a, None, [20, 20, 0]),
            usage_commit_row(second, similar_host, &b, None, [11, 5, 0]),
            local.to_string(),
            usage_commit_row(first, internal, "missing-sha", None, [1, 1, 0]),
            usage_commit_row(first, external, "missing-sha", None, [1, 1, 0]),
        ],
    );

    let scoped = usage_date_json(
        &repo,
        &metrics_db_path,
        first,
        second,
        Some("GIT.XIAOJUKEJI.COM"),
    );
    assert_eq!(
        scoped,
        json!({
            "totals": {"commit_count": 2, "added_lines": 12, "ai_lines": 6, "human_lines": 2, "unknown_lines": 4},
            "daily": [
                {"date": first.to_string(), "commit_count": 1, "added_lines": 5, "ai_lines": 2, "human_lines": 1, "unknown_lines": 2},
                {"date": second.to_string(), "commit_count": 1, "added_lines": 7, "ai_lines": 4, "human_lines": 1, "unknown_lines": 2},
            ],
            "repos": [
                {"repo": "https://git.xiaojukeji.com/team/first", "commit_count": 1, "added_lines": 5, "ai_lines": 2, "human_lines": 1, "unknown_lines": 2},
                {"repo": "https://git.xiaojukeji.com/team/second", "commit_count": 1, "added_lines": 7, "ai_lines": 4, "human_lines": 1, "unknown_lines": 2},
            ],
            "pending_events": 1,
        })
    );
    let unscoped = usage_date_json(&repo, &metrics_db_path, first, second, None);
    assert_eq!(unscoped["totals"]["added_lines"], 62);
    assert_eq!(unscoped["pending_events"], 2);
}

#[test]
fn usage_machine_scopes_duplicate_and_rewrite_evidence_without_moving_original_dates() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new();
    let first = personal_usage_date();
    let second = first.succ_opt().unwrap();
    let later = second.succ_opt().unwrap();
    let repo_a = "https://example.invalid/company/usage-a";
    let repo_b = "https://example.invalid/company/usage-b";
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let rebased = "c".repeat(40);
    let squashed = "d".repeat(40);
    let main = "e".repeat(40);
    let excluded = "f".repeat(40);
    let rebase = usage_rewrite_row(
        later,
        repo_a,
        json!({
            "v": 1, "kind": "rebase", "ref": "refs/heads/main", "before_sha": a,
            "after_sha": rebased, "complete": true, "replaced_shas": [a],
            "targets": [{"sha": rebased, "sources": [a], "stats": {"added_lines": 5, "deleted_lines": 0, "ai_lines": 2, "human_lines": 1}}],
        }),
    );
    let squash = usage_rewrite_row(
        later,
        repo_a,
        json!({
            "v": 1, "kind": "squash_merge", "ref": "refs/heads/main", "before_sha": main,
            "after_sha": squashed, "complete": true, "replaced_shas": [],
            "targets": [{"sha": squashed, "sources": [rebased, b], "stats": {"added_lines": 12, "deleted_lines": 0, "ai_lines": 6, "human_lines": 2}}],
        }),
    );
    insert_personal_rows(
        &metrics_db_path,
        &[
            usage_commit_row(first, repo_a, &a, None, [5, 2, 1]),
            usage_commit_row(second, repo_a, &b, None, [7, 4, 1]),
            // Same commit reported again the next day must not move or duplicate it.
            usage_commit_row(second, repo_a, &a, None, [5, 2, 1]),
            usage_commit_row(second, repo_a, &rebased, None, [5, 2, 1]),
            usage_commit_row(second, repo_a, &squashed, None, [12, 6, 2]),
            // A target SHA observed in another repository is an independent event.
            usage_commit_row(second, repo_b, &rebased, None, [3, 1, 1]),
            usage_commit_row(later, repo_a, &excluded, None, [100, 90, 9]),
            // Scope evidence may be observed after both the ID 1s and the query window.
            squash.clone(),
            rebase,
            squash,
        ],
    );

    let actual = usage_date_json(&repo, &metrics_db_path, first, second, None);
    assert_eq!(
        actual,
        json!({
            "totals": {"commit_count": 3, "added_lines": 15, "ai_lines": 7, "human_lines": 3, "unknown_lines": 5},
            "daily": [
                {"date": first.to_string(), "commit_count": 1, "added_lines": 5, "ai_lines": 2, "human_lines": 1, "unknown_lines": 2},
                {"date": second.to_string(), "commit_count": 2, "added_lines": 10, "ai_lines": 5, "human_lines": 2, "unknown_lines": 3},
            ],
            "repos": [
                {"repo": repo_a, "commit_count": 2, "added_lines": 12, "ai_lines": 6, "human_lines": 2, "unknown_lines": 4},
                {"repo": repo_b, "commit_count": 1, "added_lines": 3, "ai_lines": 1, "human_lines": 1, "unknown_lines": 1},
            ],
            "pending_events": 0,
        })
    );
    for field in [
        "commit_count",
        "added_lines",
        "ai_lines",
        "human_lines",
        "unknown_lines",
    ] {
        let repo_total: u64 = actual["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|repo| repo[field].as_u64().unwrap())
            .sum();
        assert_eq!(repo_total, actual["totals"][field].as_u64().unwrap());
    }
}

#[test]
fn usage_machine_reports_relevant_conflicts_and_unresolved_derivatives() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new();
    let first = personal_usage_date();
    let second = first.succ_opt().unwrap();
    let remote = "https://example.invalid/company/pending";
    let conflicted = "a".repeat(40);
    let missing_source = "b".repeat(40);
    let target = "c".repeat(40);
    let rewrite = usage_rewrite_row(
        first,
        remote,
        json!({
            "v": 1, "kind": "rebase", "ref": "refs/heads/main",
            "before_sha": missing_source, "after_sha": target,
            "complete": true, "replaced_shas": [missing_source],
            "targets": [{"sha": target, "sources": [missing_source], "stats": null}],
        }),
    );
    insert_personal_rows(
        &metrics_db_path,
        &[
            usage_commit_row(first, remote, &conflicted, None, [1, 1, 0]),
            usage_commit_row(first, remote, &conflicted, None, [2, 1, 0]),
            usage_commit_row(second, remote, &conflicted, None, [1, 1, 0]),
            rewrite,
            usage_commit_row(second, remote, &target, None, [1, 1, 0]),
        ],
    );
    let actual = usage_date_json(&repo, &metrics_db_path, second, second, None);
    assert_eq!(actual["totals"]["commit_count"], 0);
    assert_eq!(actual["daily"], json!([]));
    assert_eq!(actual["repos"], json!([]));
    assert_eq!(actual["pending_events"], 2);
}

#[test]
fn usage_machine_counts_dated_local_commits_without_owner_filtering() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new();
    let first = personal_usage_date();
    let second = first.succ_opt().unwrap();
    let remote = "https://example.invalid/company/usage";
    let other_remote = "https://example.invalid/company/other";
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let conflict = "c".repeat(40);
    let rows = [
        usage_commit_row(first, remote, &a, Some("employee-123"), [5, 2, 1]),
        usage_commit_row(second, remote, &a, Some("employee-456"), [5, 2, 1]),
        usage_commit_row(second, remote, &a, None, [5, 2, 1]),
        usage_commit_row(second, other_remote, &a, Some("employee-456"), [3, 1, 1]),
        usage_commit_row(first, remote, &b, None, [4, 1, 1]),
        usage_commit_row(second, remote, &conflict, None, [2, 1, 0]),
        usage_commit_row(second, remote, &conflict, Some("employee-123"), [3, 1, 0]),
        usage_commit_row(
            first.pred_opt().unwrap(),
            remote,
            &"d".repeat(40),
            None,
            [20, 10, 0],
        ),
        usage_commit_row(
            second.succ_opt().unwrap(),
            remote,
            &"e".repeat(40),
            None,
            [20, 10, 0],
        ),
    ];
    insert_personal_rows(&metrics_db_path, &rows);

    let actual = usage_date_json(&repo, &metrics_db_path, first, second, None);
    assert_eq!(
        actual,
        json!({
            "totals": {"commit_count": 3, "added_lines": 12, "ai_lines": 4, "human_lines": 3, "unknown_lines": 5},
            "daily": [
                {"date": first.to_string(), "commit_count": 2, "added_lines": 9, "ai_lines": 3, "human_lines": 2, "unknown_lines": 4},
                {"date": second.to_string(), "commit_count": 1, "added_lines": 3, "ai_lines": 1, "human_lines": 1, "unknown_lines": 1},
            ],
            "repos": [
                {"repo": other_remote, "commit_count": 1, "added_lines": 3, "ai_lines": 1, "human_lines": 1, "unknown_lines": 1},
                {"repo": remote, "commit_count": 2, "added_lines": 9, "ai_lines": 3, "human_lines": 2, "unknown_lines": 4},
            ],
            "pending_events": 1,
        })
    );
}
