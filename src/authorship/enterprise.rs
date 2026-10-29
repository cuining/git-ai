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

struct CommitMetadata {
    parents: Vec<String>,
    committed_at: u64,
    tree: String,
}

fn commit_metadata(
    repo: &Repository,
    shas: &[String],
) -> Result<HashMap<String, CommitMetadata>, GitAiError> {
    if shas.is_empty() {
        return Ok(HashMap::new());
    }
    if shas
        .iter()
        .any(|sha| !crate::git::repo_state::is_valid_git_oid(sha))
    {
        return Err(GitAiError::Generic(
            "local capture requires immutable commit OIDs".into(),
        ));
    }
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".into(),
        "-s".into(),
        "--no-notes".into(),
        "--no-walk".into(),
        "--format=%H%x00%P%x00%ct%x00%T".into(),
    ]);
    args.extend(shas.iter().cloned());
    let output = exec_git(&args)?;
    let mut metadata = HashMap::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let fields: Vec<_> = line.split('\0').collect();
        if let [sha, parents, timestamp, tree] = fields.as_slice() {
            metadata.insert(
                (*sha).to_owned(),
                CommitMetadata {
                    parents: parents.split_whitespace().map(str::to_owned).collect(),
                    committed_at: timestamp
                        .parse()
                        .map_err(|_| GitAiError::Generic("invalid commit timestamp".into()))?,
                    tree: (*tree).to_owned(),
                },
            );
        }
    }
    if shas.iter().any(|sha| !metadata.contains_key(sha)) {
        return Err(GitAiError::Generic("commit metadata is unavailable".into()));
    }
    Ok(metadata)
}

fn repository_key(repo: &Repository) -> String {
    format!(
        "{:x}",
        Sha256::digest(repo.common_dir().to_string_lossy().as_bytes())
    )
}

fn retained_facts(repo: &Repository) -> Result<HashMap<String, CommitFacts>, GitAiError> {
    crate::metrics::db::MetricsDatabase::global()?
        .lock()
        .map_err(|_| GitAiError::Generic("metrics database lock poisoned".into()))?
        .commit_facts_for_repository(&repository_key(repo))
}

fn inherited_source<'a>(
    retained: &'a HashMap<String, CommitFacts>,
    sha: &str,
    path: &str,
) -> Option<&'a AiSourceSnapshot> {
    let mut sha = sha;
    let mut seen = BTreeSet::new();
    while seen.insert(sha) {
        let fact = retained.get(sha)?;
        if fact.merge.as_ref().is_some_and(|merge| {
            !matches!(
                merge.decision,
                crate::metrics::enterprise::MergeDecision::Eligible
                    | crate::metrics::enterprise::MergeDecision::NotStrictMajority
                    | crate::metrics::enterprise::MergeDecision::MissingSource
            )
        }) {
            return None;
        }
        if let Some(file) = fact.files.iter().find(|file| file.path == path) {
            return file.source_snapshot.as_ref();
        }
        if fact
            .files
            .iter()
            .any(|file| file.previous_path.as_deref() == Some(path))
        {
            return None;
        }
        sha = fact.parent_shas.first()?;
    }
    None
}

pub(crate) fn capture_ordinary_commit(
    repo: &Repository,
    commit_sha: &str,
    note: &AuthorshipLog,
    working_log: &PersistedWorkingLog,
    checkpoints: &[Checkpoint],
    initial: &mut InitialAttributions,
    initial_contents: &mut HashMap<String, String>,
) -> Result<Option<CommitFacts>, GitAiError> {
    let mut metadata = commit_metadata(repo, &[commit_sha.to_owned()])?;
    let metadata = metadata
        .remove(commit_sha)
        .ok_or_else(|| GitAiError::Generic("commit metadata is unavailable".into()))?;
    let parents = metadata.parents;
    if parents.len() > 1 {
        let commit = crate::authorship::rewrite::RewriteMetricCommit::new(
            commit_sha.to_owned(),
            Vec::new(),
            crate::authorship::rewrite::RewriteMetricOperation::Merge,
        )
        .with_authorship_note(
            note.serialize_to_string()
                .map_err(|_| GitAiError::Generic("cannot capture merge ownership".into()))?,
        );
        return Ok(capture_rewrite_commits(repo, &[commit])?.pop());
    }
    let committed_at = metadata.committed_at;
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
    let retained = retained_facts(repo)?;
    let mut paths: BTreeSet<String> = diff
        .files
        .keys()
        .chain(diff.hunks_by_file.keys())
        .cloned()
        .collect();
    paths.extend(
        working_log
            .read_initial_attributions()
            .ai_sources
            .keys()
            .map(|path| renames.get(path).unwrap_or(path).clone()),
    );
    paths.extend(
        checkpoints
            .iter()
            .flat_map(|checkpoint| &checkpoint.entries)
            .map(|entry| renames.get(&entry.file).unwrap_or(&entry.file).clone()),
    );
    let mut requests = Vec::new();
    for path in &paths {
        requests.push((commit_sha.to_owned(), path.clone()));
        if let Some(parent) = parents.first() {
            let old_path = diff
                .renames
                .iter()
                .find(|(_, new)| new == path)
                .map(|(old, _)| old)
                .unwrap_or(path);
            requests.push((parent.clone(), old_path.clone()));
        }
    }
    let committed_contents = batch_read_paths_at_treeishes(repo, &requests)?;
    let mut inherited = BTreeMap::new();
    if let Some(parent) = parents.first() {
        for path in &paths {
            let old_path = diff
                .renames
                .iter()
                .find(|(_, new)| new == path)
                .map(|(old, _)| old)
                .unwrap_or(path);
            if let Some(source) = inherited_source(&retained, parent, old_path)
                && let Some(before) = committed_contents.get(&(parent.clone(), old_path.clone()))
                && source.matches(before)
            {
                inherited.insert(path.clone(), (source.clone(), before.clone()));
            }
        }
    }
    let snapshots = replay_sources(working_log, checkpoints, &renames, inherited)?;
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
    let files = file_facts(
        &diff,
        note,
        commit_sha,
        &committed_contents,
        committed_sources,
    );
    Ok(Some(CommitFacts {
        repository_key: repository_key(repo),
        repo_url: crate::repo_url::resolve_repo_url_from_repo(repo),
        commit_sha: commit_sha.to_owned(),
        parent_shas: parents,
        committed_at,
        recorded_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        legacy_event_hashes: Vec::new(),
        relations: Vec::new(),
        merge: None,
        files,
    }))
}

fn file_facts(
    diff: &crate::authorship::rewrite::DiffTreeResult,
    note: &AuthorshipLog,
    commit_sha: &str,
    committed_contents: &HashMap<(String, String), String>,
    mut committed_sources: HashMap<String, AiSourceSnapshot>,
) -> Vec<CommitFileFacts> {
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
        if let Some(file) = note.attestations.iter().find(|file| file.file_path == path) {
            let mut covered = Vec::new();
            for entry in file.entries.iter().rev() {
                let known =
                    entry.hash.starts_with("h_") && note.metadata.humans.contains_key(&entry.hash);
                let tool = note_tool(note, &entry.hash);
                if !known && tool.is_none() {
                    continue;
                }
                let ranges = entry.line_ranges.iter().map(line_range).collect::<Vec<_>>();
                let ranges = subtract_ranges(&intersect_ranges(&ranges, &added), &covered);
                covered.extend(ranges.iter().copied());
                if known {
                    known_human.extend(ranges);
                } else if let Some(tool) = tool {
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
    files
}

fn replay_sources(
    working_log: &PersistedWorkingLog,
    checkpoints: &[Checkpoint],
    renames: &HashMap<String, String>,
    sources: BTreeMap<String, (AiSourceSnapshot, String)>,
) -> Result<BTreeMap<String, (AiSourceSnapshot, String)>, GitAiError> {
    SourceHistory::read(working_log, checkpoints)?.replay(renames, sources)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceHistory {
    initial: BTreeMap<String, (AiSourceSnapshot, String)>,
    checkpoints: Vec<SourceCheckpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceCheckpoint {
    is_ai: bool,
    entries: Vec<SourceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceEntry {
    path: String,
    source: Option<AiSourceSnapshot>,
    content: String,
}

impl SourceHistory {
    pub(crate) fn read(
        working_log: &PersistedWorkingLog,
        checkpoints: &[Checkpoint],
    ) -> Result<Self, GitAiError> {
        let mut initial = BTreeMap::new();
        for (path, source) in working_log.read_initial_attributions().ai_sources {
            let content = working_log.get_file_version(&source.blob_sha)?;
            if !source.matches(&content) {
                return Err(GitAiError::Generic(
                    "invalid INITIAL source snapshot".into(),
                ));
            }
            initial.insert(path, (source, content));
        }
        let mut history = Vec::new();
        for checkpoint in checkpoints {
            let mut entries = Vec::new();
            for entry in &checkpoint.entries {
                let content = working_log.get_file_version(&entry.blob_sha)?;
                if let Some(source) = &entry.ai_source
                    && (source.blob_sha != entry.blob_sha || !source.matches(&content))
                {
                    return Err(GitAiError::Generic(
                        "invalid captured source snapshot".into(),
                    ));
                }
                entries.push(SourceEntry {
                    path: entry.file.clone(),
                    source: entry.ai_source.clone(),
                    content,
                });
            }
            history.push(SourceCheckpoint {
                is_ai: checkpoint.kind.is_ai(),
                entries,
            });
        }
        Ok(Self {
            initial,
            checkpoints: history,
        })
    }

    fn replay(
        &self,
        renames: &HashMap<String, String>,
        mut sources: BTreeMap<String, (AiSourceSnapshot, String)>,
    ) -> Result<BTreeMap<String, (AiSourceSnapshot, String)>, GitAiError> {
        let mut destinations = BTreeSet::new();
        let mut ambiguous = BTreeSet::new();
        for (path, (source, content)) in &self.initial {
            let destination = renames.get(path).unwrap_or(path).clone();
            if !destinations.insert(destination.clone()) {
                sources.remove(&destination);
                ambiguous.insert(destination);
                continue;
            }
            if !ambiguous.contains(&destination) {
                sources.insert(destination, (source.clone(), content.clone()));
            }
        }
        for checkpoint in &self.checkpoints {
            let mut entries: Vec<_> = checkpoint
                .entries
                .iter()
                .map(|entry| {
                    let destination = renames.get(&entry.path).unwrap_or(&entry.path).clone();
                    let exact_path = entry.path == destination;
                    (destination, exact_path, entry)
                })
                .collect();
            entries.sort_by(|left, right| {
                (&left.0, left.1, &left.2.path).cmp(&(&right.0, right.1, &right.2.path))
            });
            for (path, _, entry) in entries {
                let previous = sources.remove(&path);
                let Some(captured) = &entry.source else {
                    continue;
                };
                let mut source = if let Some((source, before)) = previous {
                    source.project(&before, &entry.content)?
                } else {
                    AiSourceSnapshot::capture(
                        None,
                        &entry.content,
                        &entry.content,
                        "human",
                        None,
                        0,
                    )?
                };
                if checkpoint.is_ai {
                    source = source.overlay(&captured.newly_captured())?;
                }
                sources.insert(path, (source, entry.content.clone()));
            }
        }
        Ok(sources)
    }
}

fn line_range(range: &LineRange) -> InclusiveLineRange {
    match range {
        LineRange::Single(line) => (*line, *line),
        LineRange::Range(start, end) => (*start, *end),
    }
}

fn note_tool(note: &AuthorshipLog, author: &str) -> Option<String> {
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

pub(crate) fn capture_rewrite_commits(
    repo: &Repository,
    commits: &[crate::authorship::rewrite::RewriteMetricCommit],
) -> Result<Vec<CommitFacts>, GitAiError> {
    use crate::authorship::rewrite::RewriteMetricOperation as Operation;
    use crate::metrics::enterprise::{CommitRelation, CommitRelationKind as Relation};
    let target_shas: Vec<_> = commits
        .iter()
        .map(|commit| commit.new_sha.clone())
        .collect();
    let metadata_shas: Vec<_> = target_shas
        .iter()
        .chain(commits.iter().flat_map(|commit| &commit.original_shas))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let metadata = commit_metadata(repo, &metadata_shas)?;
    let mut retained = retained_facts(repo)?;
    let ordered = parent_order(commits, &metadata)?;
    let mut pairs = BTreeSet::new();
    let mut diffs = HashMap::new();
    for commit in commits {
        let parents = &metadata[&commit.new_sha].parents;
        let base = parents
            .first()
            .cloned()
            .unwrap_or(crate::authorship::diff_base::EMPTY_TREE_SHA.into());
        let pair = (base.clone(), commit.new_sha.clone());
        if let Some(provided) = &commit.parent_diff
            && commit.parent_sha.as_deref() == Some(base.as_str())
            && !provided.files.is_empty()
            && provided
                .hunks_by_file
                .keys()
                .all(|path| provided.files.contains_key(path))
        {
            diffs.insert(pair, provided.clone());
        } else {
            pairs.insert(pair);
        }
        for source in &commit.original_shas {
            pairs.insert((source.clone(), commit.new_sha.clone()));
        }
        if parents.len() == 2 {
            pairs.insert((parents[1].clone(), commit.new_sha.clone()));
        }
    }
    let pairs: Vec<_> = pairs.into_iter().collect();
    let results = compute_diff_trees_batch(repo, &pairs, true)?;
    diffs.extend(pairs.into_iter().zip(results));
    if diffs.values().any(|diff| diff.metadata_error.is_some()) {
        return Err(GitAiError::Generic(
            "rewrite diff metadata is unavailable".into(),
        ));
    }
    let missing_notes: Vec<_> = commits
        .iter()
        .filter(|commit| commit.authorship_note.is_none())
        .map(|commit| commit.new_sha.clone())
        .chain(
            metadata
                .values()
                .filter(|meta| meta.parents.len() == 2)
                .map(|meta| meta.parents[1].clone()),
        )
        .collect();
    let notes = crate::git::notes_api::read_local_notes_batch(repo, &missing_notes)?;
    let known_paths: BTreeSet<_> = retained
        .values()
        .flat_map(|fact| &fact.files)
        .map(|file| file.path.clone())
        .collect();
    let mut requests = BTreeSet::new();
    for commit in commits {
        let meta = &metadata[&commit.new_sha];
        let base = meta
            .parents
            .first()
            .cloned()
            .unwrap_or(crate::authorship::diff_base::EMPTY_TREE_SHA.into());
        let diff = &diffs[&(base, commit.new_sha.clone())];
        for path in diff.files.keys() {
            requests.insert((commit.new_sha.clone(), path.clone()));
            for parent in &meta.parents {
                let old_path = if meta.parents.first() == Some(parent) {
                    previous_path(diff, path)
                } else if meta.parents.len() == 2 {
                    previous_path(&diffs[&(parent.clone(), commit.new_sha.clone())], path)
                } else {
                    path
                };
                requests.insert((parent.clone(), old_path.clone()));
            }
        }
        if let Some(history) = &commit.source_history {
            for path in history.initial.keys().chain(
                history
                    .checkpoints
                    .iter()
                    .flat_map(|checkpoint| checkpoint.entries.iter().map(|entry| &entry.path)),
            ) {
                let destination = commit
                    .original_shas
                    .first()
                    .and_then(|original| {
                        diffs[&(original.clone(), commit.new_sha.clone())]
                            .renames
                            .iter()
                            .find(|(old, _)| old == path)
                            .map(|(_, new)| new)
                    })
                    .unwrap_or(path);
                requests.insert((commit.new_sha.clone(), destination.clone()));
            }
        }
        for source_sha in &commit.original_shas {
            let shift = &diffs[&(source_sha.clone(), commit.new_sha.clone())];
            for path in &known_paths {
                if inherited_source(&retained, source_sha, path).is_none() {
                    continue;
                }
                let destination = shift
                    .renames
                    .iter()
                    .find(|(old, _)| old == path)
                    .map(|(_, new)| new)
                    .unwrap_or(path);
                requests.insert((source_sha.clone(), path.clone()));
                requests.insert((commit.new_sha.clone(), destination.clone()));
            }
        }
    }
    let contents = batch_read_paths_at_treeishes(repo, &requests.into_iter().collect::<Vec<_>>())?;
    let clean_merges = clean_merge_trees(repo, &metadata, &target_shas)?;
    let repo_url = crate::repo_url::resolve_repo_url_from_repo(repo);
    let repository_key = repository_key(repo);
    let recorded_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut facts = Vec::new();
    for commit in ordered {
        let meta = &metadata[&commit.new_sha];
        let base = meta
            .parents
            .first()
            .cloned()
            .unwrap_or(crate::authorship::diff_base::EMPTY_TREE_SHA.into());
        let diff = &diffs[&(base, commit.new_sha.clone())];
        if commit.operation == Operation::Merge && meta.parents.len() < 2 {
            continue;
        }
        let note = commit
            .authorship_note
            .as_ref()
            .or_else(|| notes.get(&commit.new_sha))
            .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok())
            .or_else(|| {
                if meta.parents.len() != 2 {
                    return None;
                }
                let mut note = notes
                    .get(&meta.parents[1])
                    .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok())?;
                let shift = &diffs[&(meta.parents[1].clone(), commit.new_sha.clone())];
                note.attestations = note
                    .attestations
                    .into_iter()
                    .filter_map(|mut file| {
                        if let Some((_, new)) =
                            shift.renames.iter().find(|(old, _)| old == &file.file_path)
                        {
                            file.file_path.clone_from(new);
                        }
                        if let Some(hunks) = shift.hunks_by_file.get(&file.file_path) {
                            crate::authorship::hunk_shift::apply_hunk_shifts_to_file_attestation(
                                &file, hunks,
                            )
                        } else {
                            Some(file)
                        }
                    })
                    .collect();
                Some(note)
            })
            .unwrap_or_default();
        let mut sources = HashMap::new();
        let mut replay_seeds = BTreeMap::new();
        if let Some(parent) = meta.parents.first() {
            for path in diff.files.keys() {
                let old_path = previous_path(diff, path);
                if let Some(source) = inherited_source(&retained, parent, old_path)
                    && let Some(before) = contents.get(&(parent.clone(), old_path.clone()))
                    && let Some(after) = contents.get(&(commit.new_sha.clone(), path.clone()))
                    && source.matches(before)
                {
                    sources.insert(path.clone(), source.project(before, after)?);
                    replay_seeds.insert(path.clone(), (source.clone(), before.clone()));
                }
            }
        }
        let mut replacement_complete = !commit.original_shas.is_empty();
        let mut candidates = BTreeMap::new();
        for source_sha in &commit.original_shas {
            let Some(original) = retained.get(source_sha) else {
                replacement_complete = false;
                continue;
            };
            if original
                .files
                .iter()
                .any(|file| !file.accepted.is_empty() && file.source_snapshot.is_none())
            {
                replacement_complete = false;
            }
            let shift = &diffs[&(source_sha.clone(), commit.new_sha.clone())];
            for path in &known_paths {
                let destination = shift
                    .renames
                    .iter()
                    .find(|(old, _)| old == path)
                    .map(|(_, new)| new)
                    .unwrap_or(path);
                let Some(source) = inherited_source(&retained, source_sha, path) else {
                    if original.files.iter().any(|file| file.path == *path) {
                        replacement_complete &= select_source(
                            &mut candidates,
                            destination,
                            source_sha,
                            None,
                            None,
                            &metadata,
                            &retained,
                        );
                    }
                    continue;
                };
                let Some(before) = contents
                    .get(&(source_sha.clone(), path.clone()))
                    .filter(|before| source.matches(before))
                else {
                    replacement_complete = false;
                    continue;
                };
                if commit.operation == Operation::Rebase
                    && let Some(file) = original.files.iter().find(|file| file.path == *path)
                    && !file.added.is_empty()
                {
                    let target_lines: Vec<_> = contents
                        .get(&(commit.new_sha.clone(), destination.clone()))
                        .map(|text| text.lines().collect())
                        .unwrap_or_default();
                    let original_lines: Vec<_> = before.lines().collect();
                    for &(start, end) in &file.added {
                        for number in start..=end {
                            let Some(line) = original_lines.get(number as usize - 1) else {
                                replacement_complete = false;
                                continue;
                            };
                            if target_lines.iter().filter(|target| *target == line).count() != 1 {
                                replacement_complete = false;
                            }
                        }
                    }
                }
                if let Some(after) = contents.get(&(commit.new_sha.clone(), destination.clone())) {
                    let projected = source.project(before, after)?;
                    replacement_complete &= select_source(
                        &mut candidates,
                        destination,
                        source_sha,
                        Some(projected),
                        Some((source.clone(), before.clone())),
                        &metadata,
                        &retained,
                    );
                }
            }
        }
        for (path, candidate) in candidates {
            match candidate {
                SourceCandidate::Known {
                    source: Some(source),
                    before,
                    ..
                } => {
                    sources.insert(path.clone(), source);
                    if let Some(before) = before {
                        replay_seeds.insert(path, *before);
                    }
                }
                _ => {
                    sources.remove(&path);
                    replay_seeds.remove(&path);
                }
            }
        }
        if matches!(
            commit.operation,
            Operation::Amend | Operation::CherryPickNoCommit
        ) {
            sources.clear();
            if let Some(history) = &commit.source_history {
                let mut renames: HashMap<_, _> = diff.renames.iter().cloned().collect();
                if let Some(original) = commit.original_shas.first() {
                    renames.extend(
                        diffs[&(original.clone(), commit.new_sha.clone())]
                            .renames
                            .iter()
                            .cloned(),
                    );
                }
                let mut carryover = BTreeMap::new();
                for (path, (source, before)) in history.replay(&renames, replay_seeds)? {
                    let after = contents
                        .get(&(commit.new_sha.clone(), path.clone()))
                        .map(String::as_str)
                        .unwrap_or("");
                    let projected = source.project(&before, after)?;
                    if projected.attributions != source.attributions
                        && !source.attributions.is_empty()
                    {
                        carryover.insert(path.clone(), (source, before));
                    }
                    if contents.contains_key(&(commit.new_sha.clone(), path.clone())) {
                        sources.insert(path, projected);
                    }
                }
                carry_uncommitted_sources(repo, &commit.new_sha, carryover)?;
            } else {
                replacement_complete = false;
            }
        }
        let merge = if meta.parents.len() > 1 {
            Some(merge_facts(
                &commit.new_sha,
                meta,
                diff,
                &diffs,
                &contents,
                &retained,
                &clean_merges,
                &mut sources,
            )?)
        } else {
            None
        };
        let kind = match commit.operation {
            Operation::Amend if replacement_complete => Relation::AmendReplacement,
            Operation::Rebase if replacement_complete => Relation::RebaseReplacement,
            Operation::Amend => Relation::Amend,
            Operation::Rebase => Relation::Rebase,
            Operation::CherryPick => Relation::CherryPick,
            Operation::CherryPickNoCommit => Relation::CherryPickNoCommit,
            Operation::Revert => Relation::Revert,
            Operation::SquashMerge => Relation::SquashMerge,
            Operation::UpdateRef => Relation::UpdateRef,
            Operation::NonFastForward | Operation::Merge => Relation::NonFastForward,
        };
        let mut fact = CommitFacts {
            repository_key: repository_key.clone(),
            repo_url: repo_url.clone(),
            commit_sha: commit.new_sha.clone(),
            parent_shas: meta.parents.clone(),
            committed_at: meta.committed_at,
            recorded_at,
            legacy_event_hashes: commit.legacy_event_hashes.clone(),
            files: file_facts(diff, &note, &commit.new_sha, &contents, sources),
            relations: if commit.original_shas.is_empty() {
                Vec::new()
            } else {
                vec![CommitRelation {
                    kind,
                    source_commits: commit.original_shas.clone(),
                }]
            },
            merge,
        };
        if !fact.replacement_source_available() {
            for relation in &mut fact.relations {
                relation.kind = match relation.kind {
                    Relation::AmendReplacement => Relation::Amend,
                    Relation::RebaseReplacement => Relation::Rebase,
                    kind => kind,
                };
            }
        }
        retained.insert(fact.commit_sha.clone(), fact.clone());
        facts.push(fact);
    }
    Ok(facts)
}

fn previous_path<'a>(
    diff: &'a crate::authorship::rewrite::DiffTreeResult,
    path: &'a String,
) -> &'a String {
    diff.renames
        .iter()
        .find(|(_, new)| new == path)
        .map(|(old, _)| old)
        .unwrap_or(path)
}

fn clean_merge_trees(
    repo: &Repository,
    metadata: &HashMap<String, CommitMetadata>,
    targets: &[String],
) -> Result<HashMap<String, bool>, GitAiError> {
    let mut merges: Vec<_> = metadata
        .iter()
        .filter(|(sha, meta)| targets.contains(sha) && meta.parents.len() == 2)
        .collect();
    merges.sort_by_key(|(sha, _)| *sha);
    if merges.is_empty() {
        return Ok(HashMap::new());
    }
    let mut args = repo.global_args_for_exec();
    args.extend([
        "merge-tree".into(),
        "--write-tree".into(),
        "--stdin".into(),
        "--no-messages".into(),
        "-z".into(),
    ]);
    let input: String = merges
        .iter()
        .map(|(_, meta)| format!("{} {}\n", meta.parents[0], meta.parents[1]))
        .collect();
    let output = crate::git::repository::exec_git_stdin(&args, input.as_bytes())?;
    let output = String::from_utf8(output.stdout)?;
    let mut fields = output.split('\0');
    let mut clean = HashMap::new();
    for (sha, meta) in merges {
        let status = fields.next();
        let tree = fields.next();
        // Conflict records include paths before their empty terminator.
        for field in fields.by_ref() {
            if field.is_empty() {
                break;
            }
        }
        clean.insert(
            sha.clone(),
            status == Some("1") && tree == Some(meta.tree.as_str()),
        );
    }
    Ok(clean)
}

#[allow(clippy::too_many_arguments)]
fn merge_facts(
    sha: &str,
    meta: &CommitMetadata,
    diff: &crate::authorship::rewrite::DiffTreeResult,
    diffs: &HashMap<(String, String), crate::authorship::rewrite::DiffTreeResult>,
    contents: &HashMap<(String, String), String>,
    retained: &HashMap<String, CommitFacts>,
    clean: &HashMap<String, bool>,
    sources: &mut HashMap<String, AiSourceSnapshot>,
) -> Result<crate::metrics::enterprise::MergeFacts, GitAiError> {
    use crate::metrics::enterprise::{MergeDecision as Decision, MergeFacts};
    let mut result = MergeFacts {
        related_additions: 0,
        verified_ai: 0,
        decision: Decision::UnsupportedMapping,
    };
    if meta.parents.len() != 2 {
        result.decision = Decision::Octopus;
        return Ok(result);
    }
    if !clean.get(sha).copied().unwrap_or(false) {
        result.decision = Decision::ConflictOrManualResolution;
        return Ok(result);
    }
    let side = &meta.parents[1];
    let shift = &diffs[&(side.clone(), sha.to_owned())];
    for (path, metadata) in &diff.files {
        if !eligible_file(path, metadata)
            || metadata.binary
            || metadata.new_mode.as_deref() == Some("160000")
        {
            continue;
        }
        let Some(after) = contents.get(&(sha.to_owned(), path.clone())) else {
            continue;
        };
        let side_path = previous_path(shift, path);
        let Some(before) = contents.get(&(side.clone(), side_path.clone())) else {
            result.decision = Decision::UnsupportedMapping;
            return Ok(result);
        };
        let target_lines: Vec<_> = after.lines().collect();
        let side_lines: Vec<_> = before.lines().collect();
        let mut related = Vec::new();
        let mut seen = BTreeSet::new();
        if before != after {
            for line in &side_lines {
                if !line.trim().is_empty() && !seen.insert(*line) {
                    result.decision = Decision::RepeatedOrAmbiguousImport;
                    return Ok(result);
                }
            }
        }
        let mut hunks = shift.hunks_by_file.get(path).cloned().unwrap_or_default();
        hunks.sort_by_key(|hunk| hunk.new_start);
        for &line in diff.added_lines_by_file.get(path).into_iter().flatten() {
            let mut delta = 0_i64;
            let mut changed = false;
            for hunk in &hunks {
                if hunk.new_count > 0
                    && line >= hunk.new_start
                    && line < hunk.new_start + hunk.new_count
                {
                    changed = true;
                    break;
                }
                let boundary = if hunk.new_count == 0 {
                    hunk.new_start + 1
                } else {
                    hunk.new_start + hunk.new_count
                };
                if line >= boundary {
                    delta += i64::from(hunk.new_count) - i64::from(hunk.old_count);
                }
            }
            if changed {
                continue;
            }
            let old = i64::from(line) - delta;
            if old <= 0 || target_lines.get(line as usize - 1) != side_lines.get(old as usize - 1) {
                result.decision = Decision::RepeatedOrAmbiguousImport;
                return Ok(result);
            }
            related.push((line, line));
        }
        result.related_additions += related.len() as u64;
        if let Some(source) =
            inherited_source(retained, side, side_path).filter(|source| source.matches(before))
        {
            let projected = source.project(before, after)?;
            let mut ai_lines: BTreeSet<u32> = BTreeSet::new();
            for ranges in source_lines(&projected, after, &related).values() {
                for &(start, end) in ranges {
                    ai_lines.extend(start..=end);
                }
            }
            result.verified_ai += ai_lines.len() as u64;
            let merged = if let Some(previous) = sources.remove(path) {
                previous.overlay(&projected)?
            } else {
                projected
            };
            sources.insert(path.clone(), merged);
        }
    }
    result.decision = if result.verified_ai == 0 {
        Decision::MissingSource
    } else if result.verified_ai.saturating_mul(2) > result.related_additions {
        Decision::Eligible
    } else {
        Decision::NotStrictMajority
    };
    Ok(result)
}

fn eligible_file(path: &str, metadata: &crate::authorship::rewrite::DiffFileMetadata) -> bool {
    path != ".gitmodules"
        && path.rsplit('/').next() != Some("page.lock")
        && !(metadata
            .old_mode
            .as_deref()
            .is_none_or(|mode| mode == "160000")
            && metadata
                .new_mode
                .as_deref()
                .is_none_or(|mode| mode == "160000"))
}

fn parent_order<'a>(
    commits: &'a [crate::authorship::rewrite::RewriteMetricCommit],
    metadata: &HashMap<String, CommitMetadata>,
) -> Result<Vec<&'a crate::authorship::rewrite::RewriteMetricCommit>, GitAiError> {
    let mut pending = BTreeMap::new();
    for commit in commits {
        if pending.insert(commit.new_sha.clone(), commit).is_some() {
            return Err(GitAiError::Generic(
                "conflicting local capture targets".into(),
            ));
        }
    }
    let mut ordered = Vec::new();
    while !pending.is_empty() {
        let ready = pending
            .keys()
            .find(|sha| {
                metadata[*sha]
                    .parents
                    .iter()
                    .all(|parent| !pending.contains_key(parent))
            })
            .cloned();
        let Some(ready) = ready else {
            return Err(GitAiError::Generic("cyclic local capture targets".into()));
        };
        ordered.push(pending.remove(&ready).unwrap());
    }
    Ok(ordered)
}

#[cfg(test)]
mod operation_tests {
    use super::*;
    use crate::authorship::rewrite::{RewriteMetricCommit, RewriteMetricOperation};

    #[test]
    fn capture_order_uses_immutable_parents_for_shuffled_targets() {
        let commits = vec![
            RewriteMetricCommit::new("a-child", Vec::new(), RewriteMetricOperation::Rebase),
            RewriteMetricCommit::new("z-parent", Vec::new(), RewriteMetricOperation::Rebase),
        ];
        let mut metadata = HashMap::from([
            (
                "a-child".into(),
                CommitMetadata {
                    parents: vec!["z-parent".into()],
                    committed_at: 1,
                    tree: String::new(),
                },
            ),
            (
                "z-parent".into(),
                CommitMetadata {
                    parents: vec!["base".into()],
                    committed_at: 2,
                    tree: String::new(),
                },
            ),
        ]);
        assert_eq!(
            parent_order(&commits, &metadata)
                .unwrap()
                .iter()
                .map(|commit| commit.new_sha.as_str())
                .collect::<Vec<_>>(),
            ["z-parent", "a-child"]
        );
        metadata.get_mut("z-parent").unwrap().parents = vec!["a-child".into()];
        assert!(parent_order(&commits, &metadata).is_err());
    }
}

enum SourceCandidate {
    Known {
        commit: String,
        source: Option<AiSourceSnapshot>,
        before: Option<Box<(AiSourceSnapshot, String)>>,
    },
    Ambiguous,
}

fn select_source(
    candidates: &mut BTreeMap<String, SourceCandidate>,
    path: &str,
    commit: &str,
    source: Option<AiSourceSnapshot>,
    before: Option<(AiSourceSnapshot, String)>,
    metadata: &HashMap<String, CommitMetadata>,
    retained: &HashMap<String, CommitFacts>,
) -> bool {
    if let Some(previous) = candidates.get(path) {
        let SourceCandidate::Known {
            commit: previous_commit,
            ..
        } = previous
        else {
            return false;
        };
        if source_ancestor(commit, previous_commit, metadata, retained) {
            return true;
        }
        if !source_ancestor(previous_commit, commit, metadata, retained) {
            candidates.insert(path.to_owned(), SourceCandidate::Ambiguous);
            return false;
        }
    }
    candidates.insert(
        path.to_owned(),
        SourceCandidate::Known {
            commit: commit.to_owned(),
            source,
            before: before.map(Box::new),
        },
    );
    true
}

fn source_ancestor(
    ancestor: &str,
    descendant: &str,
    metadata: &HashMap<String, CommitMetadata>,
    retained: &HashMap<String, CommitFacts>,
) -> bool {
    let mut pending = vec![descendant];
    let mut seen = BTreeSet::new();
    while let Some(sha) = pending.pop() {
        if sha == ancestor {
            return true;
        }
        if seen.insert(sha) {
            if let Some(meta) = metadata.get(sha) {
                pending.extend(meta.parents.iter().map(String::as_str));
            } else if let Some(fact) = retained.get(sha) {
                pending.extend(fact.parent_shas.iter().map(String::as_str));
            }
        }
    }
    false
}

fn subtract_ranges(
    ranges: &[InclusiveLineRange],
    covered: &[InclusiveLineRange],
) -> Vec<InclusiveLineRange> {
    let mut remaining = ranges.to_vec();
    for &(low, high) in covered {
        remaining = remaining
            .into_iter()
            .flat_map(|(start, end)| {
                if high < start || low > end {
                    return vec![(start, end)];
                }
                let mut pieces = Vec::new();
                if low > start {
                    pieces.push((start, low - 1));
                }
                if high < end {
                    pieces.push((high + 1, end));
                }
                pieces
            })
            .collect();
    }
    remaining
}

fn carry_uncommitted_sources(
    repo: &Repository,
    commit_sha: &str,
    carryover: BTreeMap<String, (AiSourceSnapshot, String)>,
) -> Result<(), GitAiError> {
    if carryover.is_empty() {
        return Ok(());
    }
    let working_log = repo.storage.working_log_for_base_commit(commit_sha)?;
    let mut initial = working_log.read_initial_attributions();
    for (path, (source, before)) in carryover {
        let content = working_log
            .initial_file_content_from(&initial, &path)?
            .unwrap_or_else(|| before.clone());
        let projected = source.project(&before, &content)?;
        if !projected.attributions.is_empty() {
            let blob = working_log.persist_file_version(&content)?;
            initial.file_blobs.insert(path.clone(), blob);
            initial.ai_sources.insert(path, projected);
        }
    }
    working_log.write_initial(initial)
}

#[cfg(test)]
mod merge_gate_tests {
    use super::*;
    use crate::authorship::rewrite::{DiffFileMetadata, DiffTreeResult};
    use crate::authorship::working_log::AgentId;
    use crate::metrics::enterprise::MergeDecision;

    #[test]
    fn raw_related_additions_include_lines_without_source_or_ownership() {
        for ai in [49, 50, 51] {
            let ai_text: String = (0..ai).map(|line| format!("source_{line}\n")).collect();
            let all: String = (0..100)
                .map(|line| {
                    if line < ai {
                        format!("source_{line}\n")
                    } else {
                        format!("unknown_{line}\n")
                    }
                })
                .collect();
            let source = AiSourceSnapshot::capture(
                None,
                "",
                &ai_text,
                "agent",
                Some(&AgentId {
                    tool: "codex".into(),
                    id: "session".into(),
                    model: String::new(),
                }),
                1,
            )
            .unwrap()
            .project(&ai_text, &all)
            .unwrap();
            let retained = HashMap::from([(
                "side".into(),
                CommitFacts {
                    repository_key: "repo".into(),
                    repo_url: None,
                    commit_sha: "side".into(),
                    parent_shas: Vec::new(),
                    committed_at: 1,
                    recorded_at: 1,
                    legacy_event_hashes: Vec::new(),
                    relations: Vec::new(),
                    merge: None,
                    files: vec![CommitFileFacts {
                        path: "a.rs".into(),
                        previous_path: None,
                        old_mode: None,
                        new_mode: Some("100644".into()),
                        binary: false,
                        added: vec![(1, 100)],
                        deleted: Vec::new(),
                        accepted: BTreeMap::new(),
                        known_human: Vec::new(),
                        source: None,
                        source_snapshot: Some(source),
                    }],
                },
            )]);
            let diff = DiffTreeResult {
                files: HashMap::from([(
                    "a.rs".into(),
                    DiffFileMetadata {
                        old_mode: None,
                        new_mode: Some("100644".into()),
                        binary: false,
                    },
                )]),
                added_lines_by_file: HashMap::from([("a.rs".into(), (1..=100).collect())]),
                ..Default::default()
            };
            let metadata = CommitMetadata {
                parents: vec!["first".into(), "side".into()],
                committed_at: 2,
                tree: String::new(),
            };
            let diffs =
                HashMap::from([(("side".into(), "merge".into()), DiffTreeResult::default())]);
            let contents = HashMap::from([
                (("side".into(), "a.rs".into()), all.clone()),
                (("merge".into(), "a.rs".into()), all),
            ]);
            let result = merge_facts(
                "merge",
                &metadata,
                &diff,
                &diffs,
                &contents,
                &retained,
                &HashMap::from([("merge".into(), true)]),
                &mut HashMap::new(),
            )
            .unwrap();
            assert_eq!(result.related_additions, 100);
            assert_eq!(result.verified_ai, ai);
            assert_eq!(
                result.decision,
                if ai == 51 {
                    MergeDecision::Eligible
                } else {
                    MergeDecision::NotStrictMajority
                }
            );
        }
    }
}
