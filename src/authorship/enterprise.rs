use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::rewrite::compute_diff_trees_batch;
use crate::authorship::source_evidence::AiSourceSnapshot;
use crate::authorship::working_log::Checkpoint;
use crate::error::GitAiError;
use crate::git::repo_storage::{InitialAttributions, PersistedWorkingLog};
use crate::git::repository::{Repository, batch_read_paths_at_treeishes, exec_git};
use crate::metrics::enterprise::{CommitFacts, CommitFileFacts, InclusiveLineRange};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn capture_ordinary_commit(
    repo: &Repository,
    commit_sha: &str,
    note: &AuthorshipLog,
    working_log: &PersistedWorkingLog,
    checkpoints: &[Checkpoint],
    initial: &mut InitialAttributions,
    initial_contents: &mut HashMap<String, String>,
) -> Result<Option<CommitFacts>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".into(),
        "-s".into(),
        "--no-notes".into(),
        "--no-walk".into(),
        "--format=%P%x00%ct".into(),
        commit_sha.into(),
    ]);
    let output = exec_git(&args)?;
    let metadata = String::from_utf8(output.stdout)?;
    let (parents, timestamp) = metadata
        .trim_end()
        .split_once('\0')
        .ok_or_else(|| GitAiError::Generic("commit metadata is unavailable".into()))?;
    let parents: Vec<String> = parents.split_whitespace().map(str::to_owned).collect();
    if parents.len() > 1 {
        return Ok(None);
    }
    let committed_at = timestamp
        .parse::<u64>()
        .map_err(|_| GitAiError::Generic("invalid commit timestamp".into()))?;
    let base = parents
        .first()
        .map(String::as_str)
        .unwrap_or(crate::authorship::diff_base::EMPTY_TREE_SHA);
    let mut diffs =
        compute_diff_trees_batch(repo, &[(base.to_owned(), commit_sha.to_owned())], true)?;
    let diff = diffs.remove(0);
    if let Some(error) = diff.metadata_error {
        return Err(GitAiError::Generic(error.into()));
    }
    let renames: HashMap<_, _> = diff.renames.iter().cloned().collect();
    let snapshots = replay_sources(working_log, checkpoints, &renames)?;
    let requests: Vec<_> = snapshots
        .keys()
        .map(|path| (commit_sha.to_owned(), path.clone()))
        .collect();
    let committed_contents = batch_read_paths_at_treeishes(repo, &requests)?;
    let mut committed_sources = HashMap::new();
    for (path, (source, before)) in snapshots {
        if let Some(content) = committed_contents.get(&(commit_sha.to_owned(), path.clone())) {
            committed_sources.insert(path.clone(), source.project(&before, content)?);
        }
        let old_path = diff
            .renames
            .iter()
            .find(|(_, new)| new == &path)
            .map(|(old, _)| old)
            .unwrap_or(&path);
        let committed_content = committed_contents.get(&(commit_sha.to_owned(), path.clone()));
        let carryover = initial_contents
            .get(&path)
            .or_else(|| {
                (committed_content != Some(&before))
                    .then(|| initial_contents.get(old_path))
                    .flatten()
            })
            .cloned();
        if let Some(content) = carryover {
            let carried = source.project(&before, &content)?;
            if !carried.attributions.is_empty() {
                initial_contents.insert(path.clone(), content);
                initial.ai_sources.insert(path, carried);
            }
        } else if committed_contents
            .get(&(commit_sha.to_owned(), path.clone()))
            .is_none_or(|content| content != &before)
        {
            let committed = committed_contents
                .get(&(commit_sha.to_owned(), path.clone()))
                .map(String::as_str)
                .unwrap_or("");
            let committed_projection = source.project(&before, committed)?;
            if committed_projection.attributions != source.attributions
                && !source.attributions.is_empty()
            {
                initial_contents.insert(path.clone(), before);
                initial.ai_sources.insert(path, source);
            }
        }
    }
    let mut files = Vec::new();
    let paths: BTreeSet<_> = diff
        .files
        .keys()
        .chain(diff.hunks_by_file.keys())
        .cloned()
        .collect();
    for path in paths {
        let metadata = diff.files.get(&path).cloned().unwrap_or_default();
        let mut added = Vec::new();
        let mut deleted = Vec::new();
        for hunk in diff.hunks_by_file.get(&path).into_iter().flatten() {
            if hunk.new_count > 0 {
                added.push((hunk.new_start, hunk.new_start + hunk.new_count - 1));
            }
            if hunk.old_count > 0 {
                deleted.push((hunk.old_start, hunk.old_start + hunk.old_count - 1));
            }
        }
        let mut accepted = BTreeMap::<String, Vec<InclusiveLineRange>>::new();
        let mut known_human = Vec::new();
        for file in note
            .attestations
            .iter()
            .filter(|file| file.file_path == path)
        {
            for entry in &file.entries {
                let ranges = entry.line_ranges.iter().map(line_range);
                let ranges = intersect_ranges(&ranges.collect::<Vec<_>>(), &added);
                if entry.hash.starts_with("h_") {
                    known_human.extend(ranges);
                } else if entry.hash != "human" {
                    let tool = note_tool(note, &entry.hash);
                    accepted.entry(tool).or_default().extend(ranges);
                }
            }
        }
        let source_snapshot = committed_sources.remove(&path);
        let source = source_snapshot.as_ref().and_then(|snapshot| {
            committed_contents
                .get(&(commit_sha.to_owned(), path.clone()))
                .map(|content| source_lines(snapshot, content, &added))
        });
        files.push(CommitFileFacts {
            previous_path: diff
                .renames
                .iter()
                .find(|(_, new)| new == &path)
                .map(|(old, _)| old.clone()),
            path,
            old_mode: metadata.old_mode,
            new_mode: metadata.new_mode,
            binary: metadata.binary,
            added,
            deleted,
            accepted,
            known_human,
            source,
            source_snapshot,
        });
    }
    Ok(Some(CommitFacts {
        repository_key: format!(
            "{:x}",
            Sha256::digest(repo.common_dir().to_string_lossy().as_bytes())
        ),
        repo_url: crate::repo_url::resolve_repo_url_from_repo(repo),
        commit_sha: commit_sha.to_owned(),
        parent_shas: parents,
        committed_at,
        recorded_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        legacy_event_hashes: Vec::new(),
        files,
    }))
}

fn replay_sources(
    working_log: &PersistedWorkingLog,
    checkpoints: &[Checkpoint],
    renames: &HashMap<String, String>,
) -> Result<BTreeMap<String, (AiSourceSnapshot, String)>, GitAiError> {
    let mut sources = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    let initial: BTreeMap<_, _> = working_log
        .read_initial_attributions()
        .ai_sources
        .into_iter()
        .collect();
    for (path, source) in initial {
        let destination = renames.get(&path).unwrap_or(&path).clone();
        if sources.contains_key(&destination) {
            sources.remove(&destination);
            ambiguous.insert(destination);
            continue;
        }
        if ambiguous.contains(&destination) {
            continue;
        }
        let content = working_log.get_file_version(&source.blob_sha)?;
        if source.matches(&content) {
            sources.insert(destination, (source, content));
        }
    }
    for checkpoint in checkpoints {
        let mut entries: Vec<_> = checkpoint
            .entries
            .iter()
            .map(|entry| {
                let destination = renames.get(&entry.file).unwrap_or(&entry.file).clone();
                let exact_path = entry.file == destination;
                (destination, exact_path, entry)
            })
            .collect();
        entries.sort_by(|left, right| {
            (&left.0, left.1, &left.2.file).cmp(&(&right.0, right.1, &right.2.file))
        });
        for (path, _, entry) in entries {
            let previous = sources.remove(&path);
            let Some(captured) = &entry.ai_source else {
                continue;
            };
            let content = working_log.get_file_version(&entry.blob_sha)?;
            if captured.blob_sha != entry.blob_sha || !captured.matches(&content) {
                return Err(GitAiError::Generic(
                    "invalid captured source snapshot".into(),
                ));
            }
            let mut source = if let Some((source, before)) = previous {
                source.project(&before, &content)?
            } else {
                AiSourceSnapshot::capture(None, &content, &content, "human", None, 0)?
            };
            if checkpoint.kind.is_ai() {
                source = source.overlay(&captured.newly_captured())?;
            }
            sources.insert(path, (source, content));
        }
    }
    Ok(sources)
}

fn line_range(range: &LineRange) -> InclusiveLineRange {
    match range {
        LineRange::Single(line) => (*line, *line),
        LineRange::Range(start, end) => (*start, *end),
    }
}

fn note_tool(note: &AuthorshipLog, author: &str) -> String {
    let session = author.split("::").next().unwrap_or(author);
    note.metadata
        .sessions
        .get(session)
        .map(|record| &record.agent_id)
        .or_else(|| {
            note.metadata
                .prompts
                .get(author)
                .map(|record| &record.agent_id)
        })
        .map(|agent| agent.tool.clone())
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn intersect_ranges(
    left: &[InclusiveLineRange],
    right: &[InclusiveLineRange],
) -> Vec<InclusiveLineRange> {
    left.iter()
        .flat_map(|&(start, end)| {
            right.iter().filter_map(move |&(low, high)| {
                let first = start.max(low);
                let last = end.min(high);
                (first <= last).then_some((first, last))
            })
        })
        .collect()
}

fn source_lines(
    snapshot: &AiSourceSnapshot,
    content: &str,
    added: &[InclusiveLineRange],
) -> BTreeMap<String, Vec<InclusiveLineRange>> {
    let mut lines = BTreeMap::<String, Vec<u32>>::new();
    let mut offset = 0;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if added
            .iter()
            .any(|&(start, end)| number >= start && number <= end)
        {
            for attr in &snapshot.attributions {
                let start = attr.start.max(offset);
                let end = attr.end.min(offset + line.len());
                if start < end
                    && content.as_bytes()[start..end]
                        .iter()
                        .any(|byte| !byte.is_ascii_whitespace())
                    && let Some(agent) = snapshot.agents.get(&attr.author_id)
                {
                    lines.entry(agent.tool.clone()).or_default().push(number);
                }
            }
        }
        offset += line.len();
    }
    lines
        .into_iter()
        .map(|(tool, mut lines)| {
            lines.sort_unstable();
            lines.dedup();
            (
                tool,
                LineRange::compress_lines(&lines)
                    .iter()
                    .map(line_range)
                    .collect(),
            )
        })
        .collect()
}
