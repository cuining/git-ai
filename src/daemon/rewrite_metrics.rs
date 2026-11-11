use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::post_commit::metric_tool_model_breakdown;
use crate::authorship::rewrite::{DiffTreeResult, RewriteMetricCommit, RewriteMetricOperation};
use crate::authorship::stats::CommitStats;
use crate::config::Config;
use crate::error::GitAiError;
use crate::git::repo_state::is_valid_git_oid;
use crate::git::repository::{Repository, exec_git_allow_nonzero_with_env};
use crate::metrics::attrs::{MetricRepositoryIdentity, attr_pos};
use crate::metrics::events::rewrite_committed_pos;
use crate::metrics::pos_encoded::{sparse_get_string, sparse_get_u32, sparse_get_vec_u32};
use crate::metrics::{EventAttributes, MetricEvent, PosEncoded, RewriteCommittedValues};
use serde::Serialize;

const MAX_OPERATION_COMMITS: usize = 128;

pub(crate) enum RewriteRange {
    Amend,
    Rebase { onto: Option<String> },
    Squash { source_head: String },
}

pub(crate) struct RewriteScope {
    pub reference: String,
    pub before: String,
    pub after: String,
    pub range: RewriteRange,
}

impl RewriteScope {
    fn operation(&self) -> RewriteMetricOperation {
        match self.range {
            RewriteRange::Amend => RewriteMetricOperation::Amend,
            RewriteRange::Rebase { .. } => RewriteMetricOperation::Rebase,
            RewriteRange::Squash { .. } => RewriteMetricOperation::SquashMerge,
        }
    }
}

pub(crate) fn spawn_rewrite_commit_metrics(
    repo: &Repository,
    metric_commits: Vec<RewriteMetricCommit>,
) {
    spawn_metrics(repo, metric_commits, None);
}

pub(crate) fn spawn_scoped_rewrite_metrics(
    repo: &Repository,
    metric_commits: Vec<RewriteMetricCommit>,
    scope: RewriteScope,
) {
    spawn_metrics(repo, metric_commits, Some(scope));
}

fn spawn_metrics(
    repo: &Repository,
    metric_commits: Vec<RewriteMetricCommit>,
    scope: Option<RewriteScope>,
) {
    if !crate::authorship::rewrite::rewrite_metrics_enabled() {
        return;
    }
    if metric_commits.is_empty() {
        return;
    }

    let repo = repo.clone();
    let build = move || build_rewrite_metrics(&repo, &metric_commits, scope.as_ref());
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(build).await;
            match result {
                Ok(events) => submit_events(events),
                Err(err) => tracing::warn!(%err, "rewrite metrics worker panicked"),
            }
        });
    } else {
        std::thread::spawn(move || submit_events(build()));
    }
}

fn submit_events(events: Vec<MetricEvent>) {
    if !events.is_empty() {
        crate::observability::log_metrics(events);
    }
}

pub(crate) fn dedupe_metric_commits(
    metric_commits: Vec<RewriteMetricCommit>,
) -> Vec<RewriteMetricCommit> {
    #[derive(Hash, PartialEq, Eq)]
    struct MetricCommitKey {
        new_sha: String,
        original_shas: Vec<String>,
        operation: crate::authorship::rewrite::RewriteMetricOperation,
        branch: Option<String>,
    }

    let mut deduped = Vec::new();
    let mut indices_by_key: HashMap<MetricCommitKey, usize> = HashMap::new();
    for commit in metric_commits {
        if commit.new_sha.is_empty() {
            continue;
        }
        let key = MetricCommitKey {
            new_sha: commit.new_sha.clone(),
            original_shas: commit.original_shas.clone(),
            operation: commit.operation,
            branch: commit.branch.clone(),
        };
        if let Some(index) = indices_by_key.get(&key).copied() {
            merge_metric_commit_context(&mut deduped[index], commit);
        } else {
            indices_by_key.insert(key, deduped.len());
            deduped.push(commit);
        }
    }
    deduped
}

fn merge_metric_commit_context(target: &mut RewriteMetricCommit, source: RewriteMetricCommit) {
    if target.parent_sha.is_none() {
        target.parent_sha = source.parent_sha;
    }
    if target.authorship_note.is_none() {
        target.authorship_note = source.authorship_note;
    }
    if target.parent_diff.is_none() {
        target.parent_diff = source.parent_diff;
    }
}

#[cfg(test)]
fn build_rewrite_metric_events(
    repo: &Repository,
    metric_commits: &[RewriteMetricCommit],
) -> Vec<MetricEvent> {
    build_rewrite_metrics(repo, metric_commits, None)
}

fn build_rewrite_metrics(
    repo: &Repository,
    metric_commits: &[RewriteMetricCommit],
    scope: Option<&RewriteScope>,
) -> Vec<MetricEvent> {
    let mut metric_commits = dedupe_metric_commits(metric_commits.to_vec());
    let oversized = scope.is_some()
        && (metric_commits.len() > MAX_OPERATION_COMMITS
            || metric_commits
                .iter()
                .map(|commit| commit.original_shas.len())
                .sum::<usize>()
                > MAX_OPERATION_COMMITS);
    if metric_commits.is_empty() {
        return Vec::new();
    }
    if let Some(branch) =
        scope.and_then(|scope| crate::authorship::rewrite::branch_name_from_ref(&scope.reference))
    {
        for commit in &mut metric_commits {
            commit.branch = Some(branch.clone());
        }
    }
    hydrate_missing_parent_shas(repo, &mut metric_commits);
    hydrate_missing_parent_diffs(repo, &mut metric_commits);
    let batch_context = RewriteMetricBatchContext::new(repo);

    let mut events = Vec::new();
    for metric_commit in &metric_commits {
        match build_rewrite_committed_metric_event(metric_commit, &batch_context) {
            Ok(Some(event)) => events.push(event),
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(
                    %err,
                    commit_sha = %metric_commit.new_sha,
                    operation_kind = metric_commit.operation.as_str(),
                    "skipping rewrite committed metric"
                );
            }
        }
    }
    // Only attach evidence to a commit produced by this operation. An empty
    // rebase can move HEAD to a destination that was already recorded as ID 1.
    if let Some(scope) = scope.filter(|scope| {
        !oversized
            && metric_commits
                .iter()
                .any(|commit| commit.new_sha == scope.after)
    }) && let Some(scoped_event) =
        build_scoped_result(repo, &metric_commits, scope, &batch_context, &events)
    {
        if let Some(tip) = events.iter_mut().find(|event| {
            sparse_get_string(&event.attrs, attr_pos::COMMIT_SHA)
                .flatten()
                .as_deref()
                == Some(scope.after.as_str())
        }) {
            tip.values.extend(scoped_event.values);
        } else {
            events.push(scoped_event);
        }
    }
    events
}

#[derive(Serialize)]
struct RewriteResult<'a> {
    v: u8,
    kind: &'static str,
    r#ref: &'a str,
    before_sha: &'a str,
    after_sha: &'a str,
    complete: bool,
    // Dated usage can reuse the original contribution only if the amend
    // changed metadata without changing the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    amend_same_tree: Option<bool>,
    replaced_shas: Vec<String>,
    targets: Vec<RewriteTarget<'a>>,
}

#[derive(Serialize)]
struct RewriteTarget<'a> {
    sha: &'a str,
    sources: &'a [String],
    stats: Option<RewriteTargetStats>,
}

#[derive(Clone, Copy, Serialize)]
struct RewriteTargetStats {
    added_lines: u32,
    deleted_lines: u32,
    ai_lines: u32,
    human_lines: u32,
}

impl RewriteTargetStats {
    fn from_event(event: &MetricEvent) -> Option<Self> {
        let count = |pos| sparse_get_u32(&event.values, pos).flatten();
        Some(Self {
            added_lines: count(rewrite_committed_pos::GIT_DIFF_ADDED_LINES)?,
            deleted_lines: count(rewrite_committed_pos::GIT_DIFF_DELETED_LINES)?,
            ai_lines: *sparse_get_vec_u32(&event.values, rewrite_committed_pos::AI_ADDITIONS)
                .flatten()?
                .first()?,
            human_lines: count(rewrite_committed_pos::HUMAN_ADDITIONS)?,
        })
    }
}

fn build_scoped_result(
    repo: &Repository,
    commits: &[RewriteMetricCommit],
    scope: &RewriteScope,
    batch: &RewriteMetricBatchContext,
    events: &[MetricEvent],
) -> Option<MetricEvent> {
    let stats_by_sha: HashMap<_, _> = events
        .iter()
        .filter_map(|event| {
            let sha = sparse_get_string(&event.attrs, attr_pos::COMMIT_SHA).flatten()?;
            Some((sha, RewriteTargetStats::from_event(event)?))
        })
        .collect();
    let replaced = verified_replacements(repo, commits, scope);
    let result = RewriteResult {
        v: 1,
        kind: scope.operation().as_str(),
        r#ref: &scope.reference,
        before_sha: &scope.before,
        after_sha: &scope.after,
        complete: replaced.is_some(),
        amend_same_tree: match scope.range {
            RewriteRange::Amend => commit_trees(repo, &scope.before, &scope.after)
                .map(|(before, after)| before == after),
            _ => None,
        },
        replaced_shas: replaced.unwrap_or_default(),
        targets: commits
            .iter()
            .map(|commit| RewriteTarget {
                sha: &commit.new_sha,
                sources: &commit.original_shas,
                stats: stats_by_sha.get(&commit.new_sha).copied(),
            })
            .collect(),
    };
    let payload = serde_json::to_string(&result).ok()?;
    let branch = crate::authorship::rewrite::branch_name_from_ref(&scope.reference)?;
    let tip =
        RewriteMetricCommit::new(&scope.after, Vec::new(), scope.operation()).with_branch(branch);
    let mut attrs = rewrite_metric_attrs(&tip, batch);
    if let Some(parent) = commits
        .iter()
        .find(|commit| commit.new_sha == scope.after)
        .and_then(|commit| commit.parent_sha.as_deref())
    {
        attrs = attrs.base_commit_sha(parent);
    }
    Some(MetricEvent::from_values(
        RewriteCommittedValues::new()
            .operation_kind(scope.operation().as_str())
            .rewrite_result(payload),
        attrs.to_sparse(),
    ))
}

fn verified_replacements(
    repo: &Repository,
    commits: &[RewriteMetricCommit],
    scope: &RewriteScope,
) -> Option<Vec<String>> {
    if !commits.iter().any(|commit| commit.new_sha == scope.after) {
        return None;
    }
    match &scope.range {
        RewriteRange::Amend => (commits.len() == 1
            && commits[0].original_shas == [scope.before.as_str()])
        .then(|| vec![scope.before.clone()]),
        RewriteRange::Rebase { onto: Some(onto) } => {
            // Independently check range membership. range-diff may guess how a
            // dropped commit maps to another target. Only complete one-to-one
            // mappings are used as replacement evidence here.
            if commits.iter().any(|commit| commit.original_shas.len() != 1) {
                return None;
            }
            let mut args = repo.global_args_for_exec();
            args.extend([
                "rev-list".to_string(),
                "--left-right".to_string(),
                format!("--max-count={}", 2 * MAX_OPERATION_COMMITS + 1),
                format!("{}...{}", scope.before, scope.after),
                format!("^{onto}"),
            ]);
            let output = crate::git::repository::exec_git(&args).ok()?;
            let stdout = std::str::from_utf8(&output.stdout).ok()?;
            let mut left = Vec::new();
            let mut right = HashSet::new();
            for line in stdout.lines() {
                if let Some(sha) = line.strip_prefix('<') {
                    left.push(sha.to_string());
                } else if let Some(sha) = line.strip_prefix('>') {
                    right.insert(sha);
                } else {
                    return None;
                }
            }
            let sources: HashSet<&str> = commits
                .iter()
                .map(|commit| commit.original_shas[0].as_str())
                .collect();
            let targets: HashSet<&str> = commits
                .iter()
                .map(|commit| commit.new_sha.as_str())
                .collect();
            if left.len() != commits.len()
                || sources.len() != commits.len()
                || targets.len() != commits.len()
                || !left.iter().all(|sha| sources.contains(sha.as_str()))
                || right != targets
            {
                return None;
            }
            left.sort_unstable();
            Some(left)
        }
        RewriteRange::Rebase { onto: None } => None,
        RewriteRange::Squash { source_head } => {
            let [target] = commits else { return None };
            if target.parent_sha.as_deref() != Some(scope.before.as_str())
                || !target.original_shas.contains(source_head)
            {
                return None;
            }
            // A pending squash is matched to a later commit. Verify its tree
            // to distinguish the expected merge from an abandoned squash or
            // a commit with additional changes.
            let (source_tree, after_tree) = commit_trees(repo, source_head, &scope.after)?;
            // A squash merge copies source commits into the destination. It
            // never establishes that the source ref stopped counting them.
            (source_tree == after_tree
                || squash_matches_merge_tree(repo, &scope.before, source_head, &after_tree)?)
            .then(Vec::new)
        }
    }
}

fn commit_trees(repo: &Repository, first: &str, second: &str) -> Option<(String, String)> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-parse".to_string(),
        format!("{first}^{{tree}}"),
        format!("{second}^{{tree}}"),
    ]);
    let output = crate::git::repository::exec_git(&args).ok()?;
    let mut trees = std::str::from_utf8(&output.stdout).ok()?.lines();
    let first_tree = trees.next()?;
    let second_tree = trees.next()?;
    (trees.next().is_none()).then(|| (first_tree.to_owned(), second_tree.to_owned()))
}

fn squash_matches_merge_tree(
    repo: &Repository,
    before: &str,
    source: &str,
    after_tree: &str,
) -> Option<bool> {
    if !is_valid_git_oid(before) || !is_valid_git_oid(source) || before.len() != source.len() {
        return None;
    }
    let object_format = if before.len() == 64 { "sha256" } else { "sha1" };
    let objects = repo.common_dir().join("objects");
    let objects = objects.to_str()?;
    if objects.contains(['\n', '\r']) {
        return None;
    }

    // merge-tree can write objects and run configured merge drivers. Isolate
    // both the output objects and Git config from the original repository.
    let scratch = tempfile::tempdir().ok()?;
    let scratch_path = scratch.path().to_str()?;
    let empty_config = scratch.path().join("no-user-config");
    let env = [
        ("GIT_CONFIG_NOSYSTEM", OsStr::new("1")),
        ("GIT_CONFIG_GLOBAL", empty_config.as_os_str()),
        ("GIT_CONFIG_COUNT", OsStr::new("0")),
        ("GIT_CONFIG_PARAMETERS", OsStr::new("")),
        ("GIT_ATTR_NOSYSTEM", OsStr::new("1")),
        ("GIT_ATTR_SOURCE", OsStr::new(before)),
    ];
    let init_args = [
        "init".to_owned(),
        "--bare".to_owned(),
        "--quiet".to_owned(),
        "--template=".to_owned(),
        format!("--object-format={object_format}"),
        scratch_path.to_owned(),
    ];
    if !exec_git_allow_nonzero_with_env(&init_args, &env)
        .ok()?
        .status
        .success()
    {
        return None;
    }
    std::fs::write(
        scratch.path().join("objects/info/alternates"),
        format!("{objects}\n"),
    )
    .ok()?;

    let merge_args = [
        format!("--git-dir={scratch_path}"),
        "-c".to_owned(),
        format!("core.attributesFile={}", empty_config.to_str()?),
        "merge-tree".to_owned(),
        "--write-tree".to_owned(),
        "--no-messages".to_owned(),
        before.to_owned(),
        source.to_owned(),
    ];
    let output = exec_git_allow_nonzero_with_env(&merge_args, &env).ok()?;
    Some(
        output.status.success()
            && std::str::from_utf8(&output.stdout).ok()?.trim_end() == after_tree,
    )
}

fn hydrate_missing_parent_shas(repo: &Repository, metric_commits: &mut [RewriteMetricCommit]) {
    let mut new_shas = Vec::new();
    let mut seen = HashSet::new();
    for metric_commit in metric_commits.iter() {
        if metric_commit.parent_sha.is_some() {
            continue;
        }
        if seen.insert(metric_commit.new_sha.clone()) {
            new_shas.push(metric_commit.new_sha.clone());
        }
    }
    if new_shas.is_empty() {
        return;
    }

    let Some(parent_by_commit) = parent_shas_for_commits(repo, &new_shas) else {
        return;
    };
    for metric_commit in metric_commits {
        if metric_commit.parent_sha.is_none()
            && let Some(parent_sha) = parent_by_commit.get(&metric_commit.new_sha)
        {
            metric_commit.parent_sha = Some(parent_sha.clone());
        }
    }
}

fn parent_shas_for_commits(
    repo: &Repository,
    commit_shas: &[String],
) -> Option<HashMap<String, String>> {
    if commit_shas.is_empty() {
        return Some(HashMap::new());
    }

    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%H %P".to_string(),
        "--no-walk".to_string(),
    ]);
    args.extend(commit_shas.iter().cloned());

    let output = crate::git::repository::exec_git(&args).ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parent_by_commit = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(commit_sha) = parts.next() else {
            continue;
        };
        let parents = parts.collect::<Vec<_>>();
        match parents.as_slice() {
            [] => {
                parent_by_commit.insert(commit_sha.to_string(), "initial".to_string());
            }
            [parent_sha] => {
                parent_by_commit.insert(commit_sha.to_string(), (*parent_sha).to_string());
            }
            _ => {
                // Existing rewrite metrics skip merge commits.
            }
        }
    }
    Some(parent_by_commit)
}

struct RewriteMetricBatchContext {
    ignore_patterns: Vec<String>,
    repository: MetricRepositoryIdentity,
    custom_attributes_json: Option<String>,
}

impl RewriteMetricBatchContext {
    fn new(repo: &Repository) -> Self {
        Self {
            ignore_patterns: effective_ignore_patterns(repo, &[], &[]),
            repository: MetricRepositoryIdentity::from_repo(repo),
            custom_attributes_json: rewrite_metric_custom_attributes_json(),
        }
    }
}

fn rewrite_metric_custom_attributes_json() -> Option<String> {
    let config = Config::fresh();
    let attrs = config.custom_attributes();
    if attrs.is_empty() {
        None
    } else {
        serde_json::to_string(attrs).ok()
    }
}

fn hydrate_missing_parent_diffs(repo: &Repository, metric_commits: &mut [RewriteMetricCommit]) {
    let mut indices = Vec::new();
    let mut pairs = Vec::new();
    for (index, metric_commit) in metric_commits.iter().enumerate() {
        if metric_commit.parent_diff.is_some() {
            continue;
        }
        let Some(parent_sha) = metric_commit.parent_sha.as_ref() else {
            continue;
        };
        indices.push(index);
        pairs.push((parent_sha.clone(), metric_commit.new_sha.clone()));
    }
    if pairs.is_empty() {
        return;
    }

    let Ok(results) = crate::authorship::rewrite::compute_diff_trees_batch(repo, &pairs) else {
        return;
    };
    for (index, result) in indices.into_iter().zip(results) {
        metric_commits[index].parent_diff = Some(result);
    }
}

fn build_rewrite_committed_metric_event(
    metric_commit: &RewriteMetricCommit,
    batch_context: &RewriteMetricBatchContext,
) -> Result<Option<MetricEvent>, GitAiError> {
    let Some(stats) = rewrite_commit_stats(metric_commit, batch_context) else {
        return Ok(None);
    };
    let Some(breakdown) = metric_tool_model_breakdown(&stats) else {
        return Ok(None);
    };

    let mut values = RewriteCommittedValues::new()
        .human_additions(stats.human_additions)
        .git_diff_deleted_lines(stats.git_diff_deleted_lines)
        .git_diff_added_lines(stats.git_diff_added_lines)
        .tool_model_pairs(breakdown.tool_model_pairs)
        .ai_additions(breakdown.ai_additions)
        .ai_accepted(breakdown.ai_accepted)
        .authorship_note(metric_commit.authorship_note.clone().unwrap_or_default())
        .operation_kind(metric_commit.operation.as_str())
        .original_commit_shas(metric_commit.original_shas.clone());

    values = values.commit_subject_null().commit_body_null().hunks_null();

    let attrs = rewrite_metric_attrs(metric_commit, batch_context);

    Ok(Some(MetricEvent::from_values(values, attrs.to_sparse())))
}

fn rewrite_commit_stats(
    metric_commit: &RewriteMetricCommit,
    batch: &RewriteMetricBatchContext,
) -> Option<CommitStats> {
    let raw_note = metric_commit.authorship_note.as_deref()?;
    let authorship_log = AuthorshipLog::deserialize_from_string(raw_note).ok()?;
    let diff_hunks = diff_hunks_from_diff_tree_result(metric_commit.parent_diff.as_ref()?);
    if should_skip_rewrite_metric_stats(&diff_hunks, &batch.ignore_patterns) {
        return None;
    }
    Some(
        crate::authorship::stats::stats_for_commit_stats_from_hunks_with_merge_flag(
            &batch.ignore_patterns,
            &diff_hunks,
            Some(&authorship_log),
            false,
        ),
    )
}

fn diff_hunks_from_diff_tree_result(
    result: &DiffTreeResult,
) -> Vec<crate::commands::diff::DiffHunk> {
    let mut hunks = Vec::new();
    for (file_path, file_hunks) in &result.hunks_by_file {
        for hunk in file_hunks {
            hunks.push(crate::commands::diff::DiffHunk {
                file_path: file_path.clone(),
                old_file_path: None,
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                deleted_lines: line_numbers(hunk.old_start, hunk.old_count),
                added_lines: line_numbers(hunk.new_start, hunk.new_count),
                deleted_contents: Vec::new(),
                added_contents: Vec::new(),
            });
        }
    }
    hunks
}

fn line_numbers(start: u32, count: u32) -> Vec<u32> {
    if count == 0 {
        return Vec::new();
    }
    (start..start.saturating_add(count))
        .filter(|line| *line > 0)
        .collect()
}

fn should_skip_rewrite_metric_stats(
    hunks: &[crate::commands::diff::DiffHunk],
    ignore_patterns: &[String],
) -> bool {
    let ignore_matcher = crate::authorship::ignore::build_ignore_matcher(ignore_patterns);
    let mut files_with_additions = std::collections::HashSet::new();
    let mut added_lines = 0usize;
    let mut deleted_lines = 0usize;
    let mut hunk_ranges = 0usize;

    for hunk in hunks {
        if crate::authorship::ignore::should_ignore_file_with_matcher(
            &hunk.file_path,
            &ignore_matcher,
        ) {
            continue;
        }
        if !hunk.added_lines.is_empty() {
            files_with_additions.insert(hunk.file_path.as_str());
            hunk_ranges += 1;
        }
        added_lines += hunk.added_lines.len();
        deleted_lines += hunk.deleted_lines.len();
    }

    hunk_ranges >= crate::authorship::post_commit::STATS_SKIP_MAX_HUNKS
        || added_lines >= crate::authorship::post_commit::STATS_SKIP_MAX_ADDED_LINES
        || files_with_additions.len()
            >= crate::authorship::post_commit::STATS_SKIP_MAX_FILES_WITH_ADDITIONS
        || deleted_lines >= crate::authorship::post_commit::STATS_SKIP_MAX_DELETED_LINES
}

fn rewrite_metric_attrs(
    metric_commit: &RewriteMetricCommit,
    batch_context: &RewriteMetricBatchContext,
) -> EventAttributes {
    let base_commit_sha = metric_commit.parent_sha.as_deref().unwrap_or("initial");
    let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .commit_sha(metric_commit.new_sha.clone())
        .base_commit_sha(base_commit_sha);

    attrs = apply_rewrite_metric_branch(attrs, metric_commit);

    attrs = batch_context.repository.apply_to(attrs);

    attrs = apply_rewrite_metric_custom_attributes(
        attrs,
        batch_context.custom_attributes_json.as_deref(),
    );

    attrs
}

fn apply_rewrite_metric_custom_attributes(
    attrs: EventAttributes,
    custom_attributes_json: Option<&str>,
) -> EventAttributes {
    if let Some(custom_attributes_json) = custom_attributes_json {
        // `custom_attributes_map` serializes the map and stores this same string field.
        // Rewrite metrics pre-serialize once per batch to avoid repeated serde work.
        attrs.custom_attributes(custom_attributes_json)
    } else {
        attrs
    }
}

fn apply_rewrite_metric_branch(
    attrs: EventAttributes,
    metric_commit: &RewriteMetricCommit,
) -> EventAttributes {
    if let Some(branch) = metric_commit.branch.as_deref() {
        return attrs.branch(branch);
    }

    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log::LineRange;
    use crate::authorship::authorship_log_serialization::AttestationEntry;
    use crate::authorship::rewrite::RewriteMetricOperation;
    use crate::authorship::working_log::AgentId;
    use crate::metrics::EventValues;
    use std::collections::HashMap;

    fn metric_commit(
        new_sha: &str,
        originals: &[&str],
        operation: RewriteMetricOperation,
    ) -> RewriteMetricCommit {
        RewriteMetricCommit::new(
            new_sha.to_string(),
            originals.iter().map(|s| s.to_string()).collect(),
            operation,
        )
    }

    #[test]
    fn empty_rebase_mapping_emits_no_evidence_for_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = crate::git::repository::discover_repository_in_path_no_git_exec(dir).unwrap();
        let after = "a".repeat(40);
        let scope = RewriteScope {
            reference: "refs/heads/main".into(),
            before: "b".repeat(40),
            after: after.clone(),
            range: RewriteRange::Rebase { onto: Some(after) },
        };
        assert!(build_rewrite_metrics(&repo, &[], Some(&scope)).is_empty());
    }

    fn note_for_ai_line(file_path: &str, line: u32) -> String {
        let prompt_id = "prompt1".to_string();
        let mut log = AuthorshipLog::new();
        log.metadata.prompts.insert(
            prompt_id.clone(),
            crate::authorship::authorship_log::PromptRecord {
                agent_id: AgentId {
                    tool: "codex".to_string(),
                    id: "session".to_string(),
                    model: "gpt-5".to_string(),
                },
                human_author: None,
                messages_url: None,
                total_additions: 0,
                total_deletions: 0,
                accepted_lines: 0,
                overriden_lines: 0,
                custom_attributes: None,
            },
        );
        log.get_or_create_file(file_path)
            .add_entry(AttestationEntry::new(
                prompt_id,
                vec![LineRange::Single(line)],
            ));
        log.serialize_to_string().expect("serialize note")
    }

    #[test]
    fn dedupe_metric_commits_keeps_distinct_original_sets() {
        let first = metric_commit("new", &["old1"], RewriteMetricOperation::Rebase);
        let second = metric_commit("new", &["old1"], RewriteMetricOperation::Rebase);
        let squash = metric_commit(
            "new",
            &["old1", "old2"],
            RewriteMetricOperation::SquashMerge,
        );

        let result = dedupe_metric_commits(vec![first.clone(), second, squash.clone()]);

        assert_eq!(result, vec![first, squash]);
    }

    #[test]
    fn rewrite_event_schema_does_not_emit_position_10() {
        let values = RewriteCommittedValues::new()
            .human_additions(1)
            .git_diff_deleted_lines(2)
            .git_diff_added_lines(3)
            .tool_model_pairs(vec!["all".to_string()])
            .ai_additions(vec![1])
            .ai_accepted(vec![1])
            .commit_subject("subject")
            .commit_body_null()
            .authorship_note("note")
            .hunks("[]")
            .operation_kind("rebase")
            .original_commit_shas(vec!["old".to_string()]);

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(
            RewriteCommittedValues::event_id(),
            crate::metrics::types::MetricEventId::RewriteCommitted
        );
        assert!(!sparse.contains_key("10"));
        assert_eq!(
            sparse.get(&rewrite_committed_pos::OPERATION_KIND.to_string()),
            Some(&serde_json::json!("rebase"))
        );
        assert_eq!(
            sparse.get(&rewrite_committed_pos::ORIGINAL_COMMIT_SHAS.to_string()),
            Some(&serde_json::json!(["old"]))
        );
    }

    #[test]
    fn rewrite_metric_branch_overrides_head_branch_attr() {
        let commit = metric_commit("new", &["old"], RewriteMetricOperation::NonFastForward)
            .with_branch("feature");
        let attrs = apply_rewrite_metric_branch(
            crate::metrics::EventAttributes::with_version("test").branch("main"),
            &commit,
        );
        let sparse = attrs.to_sparse();

        assert_eq!(
            sparse.get(&crate::metrics::attrs::attr_pos::BRANCH.to_string()),
            Some(&serde_json::json!("feature"))
        );
    }

    #[test]
    fn rewrite_metric_custom_attributes_match_map_builder_wire_format() {
        let mut custom_attributes = HashMap::new();
        custom_attributes.insert("team".to_string(), "metrics".to_string());
        let custom_attributes_json =
            serde_json::to_string(&custom_attributes).expect("serialize custom attributes");

        let sparse_from_batch_json = apply_rewrite_metric_custom_attributes(
            crate::metrics::EventAttributes::with_version("test"),
            Some(&custom_attributes_json),
        )
        .to_sparse();
        let sparse_from_map = crate::metrics::EventAttributes::with_version("test")
            .custom_attributes_map(&custom_attributes)
            .to_sparse();

        assert_eq!(
            sparse_from_batch_json
                .get(&crate::metrics::attrs::attr_pos::CUSTOM_ATTRIBUTES.to_string()),
            sparse_from_map.get(&crate::metrics::attrs::attr_pos::CUSTOM_ATTRIBUTES.to_string())
        );
    }

    #[test]
    fn rewrite_metric_event_uses_supplied_note_and_parent_diff() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        let note = note_for_ai_line("file.txt", 1);

        let mut hunks_by_file = HashMap::new();
        hunks_by_file.insert(
            "file.txt".to_string(),
            vec![crate::authorship::hunk_shift::DiffHunk {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
            }],
        );
        let parent_diff = DiffTreeResult {
            hunks_by_file,
            added_lines_by_file: HashMap::new(),
            renames: Vec::new(),
        };
        let commit = metric_commit("new", &["old"], RewriteMetricOperation::Rebase)
            .with_branch("feature")
            .with_parent_sha("parent")
            .with_authorship_note(note.clone())
            .with_parent_diff(parent_diff);

        let batch_context = RewriteMetricBatchContext::new(tmp.gitai_repo());
        let event = build_rewrite_committed_metric_event(&commit, &batch_context)
            .expect("metric build")
            .expect("event");

        assert_eq!(
            event
                .values
                .get(&rewrite_committed_pos::AUTHORSHIP_NOTE.to_string()),
            Some(&serde_json::json!(note))
        );
        assert_eq!(
            event
                .values
                .get(&rewrite_committed_pos::GIT_DIFF_ADDED_LINES.to_string()),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            event
                .values
                .get(&rewrite_committed_pos::TOOL_MODEL_PAIRS.to_string()),
            Some(&serde_json::json!(["all", "codex::gpt-5"]))
        );
        assert_eq!(
            event
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BRANCH.to_string()),
            Some(&serde_json::json!("feature"))
        );
        assert_eq!(
            event
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::json!("parent"))
        );
    }

    #[test]
    fn rewrite_metric_worker_hydrates_missing_parent_and_diff() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "base\n", false)
            .expect("write base");
        let parent_sha = tmp.commit_all("base").expect("base commit");
        tmp.write_file("file.txt", "base\nai\n", false)
            .expect("write update");
        let new_sha = tmp.commit_all("update").expect("update commit");
        let note = note_for_ai_line("file.txt", 2);

        let commit = metric_commit(
            &new_sha,
            &[&parent_sha],
            RewriteMetricOperation::NonFastForward,
        )
        .with_authorship_note(note);

        let events = build_rewrite_metric_events(tmp.gitai_repo(), &[commit]);

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::json!(parent_sha))
        );
        assert_eq!(
            events[0]
                .values
                .get(&rewrite_committed_pos::GIT_DIFF_ADDED_LINES.to_string()),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn rewrite_metric_worker_hydrates_initial_parent_diff() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "ai\n", false)
            .expect("write root");
        let root_sha = tmp.commit_all("root").expect("root commit");
        let note = note_for_ai_line("file.txt", 1);

        let commit = metric_commit(&root_sha, &["old"], RewriteMetricOperation::Amend)
            .with_authorship_note(note);

        let events = build_rewrite_metric_events(tmp.gitai_repo(), &[commit]);

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::json!("initial"))
        );
        assert_eq!(
            events[0]
                .values
                .get(&rewrite_committed_pos::GIT_DIFF_ADDED_LINES.to_string()),
            Some(&serde_json::json!(1))
        );
    }
}
