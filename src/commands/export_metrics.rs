//! Offline enterprise export of already recorded metrics. This module never
//! starts a daemon, refreshes pricing, reads transcripts, or contacts an API.

use crate::authorship::authorship_log_serialization::{AuthorshipLog, generate_session_id};
use crate::commands::diff::DiffJsonHunk;
use crate::metrics::db::MetricsDatabase;
use crate::metrics::enterprise::CommitFacts;
use crate::metrics::events::{committed_pos, rewrite_committed_pos, token_usage_pos};
use crate::metrics::pos_encoded::{sparse_get_string, sparse_get_u64};
use crate::metrics::{MetricEvent, attrs::attr_pos};
use chrono::{DateTime, Utc};
use clap::Parser;
use glob::Pattern;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "git-ai export-metrics",
    about = "Export local code and token metrics without uploading"
)]
struct Options {
    /// Enterprise identity of the owner of this local metrics database.
    #[arg(long)]
    username: String,
    /// Source SQLite database; defaults to ~/.git-ai/internal/metrics-db.
    #[arg(long)]
    database: Option<PathBuf>,
    /// Classification JSON containing test_patterns and language_extensions.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Inclusive RFC3339 commit time / token bucket start.
    #[arg(long)]
    since: Option<String>,
    /// Exclusive RFC3339 commit time / token bucket start.
    #[arg(long)]
    until: Option<String>,
    /// Create a new JSON file; existing files are never overwritten. Default: stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Classification {
    test_patterns: Vec<String>,
    language_extensions: BTreeMap<String, String>,
}

impl Default for Classification {
    fn default() -> Self {
        Self {
            test_patterns: [
                "**/test/**",
                "**/tests/**",
                "**/__tests__/**",
                "**/spec/**",
                "**/specs/**",
                "test_*",
                "*_test.*",
                "*.test.*",
                "*.spec.*",
                "*Test.java",
                "*Tests.java",
                "*Spec.java",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            language_extensions: [
                ("rs", "Rust"),
                ("py", "Python"),
                ("java", "Java"),
                ("go", "Go"),
                ("js", "JavaScript"),
                ("jsx", "JavaScript"),
                ("ts", "TypeScript"),
                ("tsx", "TypeScript"),
                ("c", "C"),
                ("cpp", "C++"),
                ("cc", "C++"),
                ("cs", "C#"),
                ("rb", "Ruby"),
                ("kt", "Kotlin"),
                ("swift", "Swift"),
                ("md", "Markdown"),
                ("txt", "Text"),
                ("sh", "Shell"),
                ("sql", "SQL"),
                ("json", "JSON"),
                ("yaml", "YAML"),
                ("yml", "YAML"),
                ("toml", "TOML"),
            ]
            .into_iter()
            .map(|(extension, language)| (extension.to_owned(), language.to_owned()))
            .collect(),
        }
    }
}

struct Classifier {
    config: Classification,
    patterns: Vec<Pattern>,
}

impl Classifier {
    fn load(path: Option<&Path>) -> Result<Self, String> {
        let mut config = Classification::default();
        if let Some(path) = path {
            let supplied: Classification = serde_json::from_slice(
                &std::fs::read(path).map_err(|e| e.to_string())?,
            )
            .map_err(|e| {
                format!(
                    "invalid classification configuration at line {} column {}",
                    e.line(),
                    e.column()
                )
            })?;
            config.test_patterns = supplied.test_patterns;
            config
                .language_extensions
                .extend(supplied.language_extensions);
        }
        let patterns = config
            .test_patterns
            .iter()
            .map(|pattern| Pattern::new(pattern).map_err(|e| format!("invalid test pattern: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        if config
            .language_extensions
            .iter()
            .any(|(extension, language)| {
                extension.is_empty()
                    || extension.starts_with('.')
                    || extension.contains('/')
                    || extension.contains('\\')
                    || extension != &extension.to_ascii_lowercase()
                    || language.trim().is_empty()
            })
        {
            return Err("language_extensions requires nonempty lowercase extensions without leading dots or paths and nonempty language names".into());
        }
        Ok(Self { config, patterns })
    }

    fn is_test(&self, path: &str) -> bool {
        let filename = path.rsplit('/').next().unwrap_or(path);
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(path) || pattern.matches(filename))
    }

    fn language(&self, path: &str) -> &str {
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| {
                self.config
                    .language_extensions
                    .get(&extension.to_ascii_lowercase())
            })
            .map(String::as_str)
            .unwrap_or("Unknown")
    }
}

#[derive(Default, Debug, Serialize)]
struct CodeCounts {
    prod_code_line_num: u64,
    prod_code_delete_line_num: u64,
    prod_ai_code_line_gen_num: Option<u64>,
    prod_ai_code_line_accept_num: u64,
    prod_ai_surviving_source_line_num: Option<u64>,
    test_code_line_num: u64,
    test_code_delete_line_num: u64,
    test_ai_code_line_gen_num: Option<u64>,
    test_ai_code_line_accept_num: u64,
    test_ai_surviving_source_line_num: Option<u64>,
}

impl CodeCounts {
    fn add(&mut self, test: bool, added: u64, deleted: u64, ai: u64) {
        if test {
            self.test_code_line_num += added;
            self.test_code_delete_line_num += deleted;
            self.test_ai_code_line_accept_num += ai;
        } else {
            self.prod_code_line_num += added;
            self.prod_code_delete_line_num += deleted;
            self.prod_ai_code_line_accept_num += ai;
        }
    }
}

#[derive(Default, Debug, Serialize)]
struct AgentCounts {
    agent_type: String,
    prod_ai_code_line_gen_num: Option<u64>,
    prod_ai_code_line_accept_num: u64,
    prod_ai_surviving_source_line_num: Option<u64>,
    test_ai_code_line_gen_num: Option<u64>,
    test_ai_code_line_accept_num: u64,
    test_ai_surviving_source_line_num: Option<u64>,
    changed_file_num: usize,
}

#[derive(Debug, Serialize)]
struct CommitRecord {
    repo: Option<String>,
    branch: Option<String>,
    commit: String,
    source_commit: Option<String>,
    source_commits: Vec<String>,
    user_name: String,
    create_at: String,
    timestamp: u64,
    timestamp_source: &'static str,
    recorded_at: u64,
    parent_commits: Option<Vec<String>>,
    diff_coverage: &'static str,
    source_evidence: &'static str,
    merge_eligibility: Option<crate::metrics::enterprise::MergeFacts>,
    #[serde(flatten)]
    counts: CodeCounts,
    changed_file_num: usize,
    ai_changed_file_num: usize,
    known_human_line_num: u64,
    unknown_line_num: u64,
    language_stats: String,
    ai_breakdown: Vec<AgentCounts>,
}

#[derive(Default)]
struct FileCounts {
    deleted_path: Option<String>,
    added: Vec<(u32, u32)>,
    deleted: Vec<(u32, u32)>,
    ai: Vec<(u32, u32)>,
    human: Vec<(u32, u32)>,
    agents: BTreeMap<String, Vec<(u32, u32)>>,
    sources: BTreeMap<String, Vec<(u32, u32)>>,
}

fn range_count(ranges: &mut [(u32, u32)]) -> u64 {
    ranges.sort_unstable();
    let mut total = 0;
    let mut current: Option<(u32, u32)> = None;
    for &(start, end) in ranges.iter() {
        match current {
            Some((low, high)) if u64::from(start) <= u64::from(high) + 1 => {
                current = Some((low, high.max(end)))
            }
            Some((low, high)) => {
                total += u64::from(high) - u64::from(low) + 1;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    total + current.map_or(0, |(low, high)| u64::from(high) - u64::from(low) + 1)
}

fn ranges_overlap(left: &[(u32, u32)], right: &[(u32, u32)]) -> bool {
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        if left[a].0 <= right[b].1 && right[b].0 <= left[a].1 {
            return true;
        }
        if left[a].1 < right[b].0 {
            a += 1;
        } else {
            b += 1;
        }
    }
    false
}

fn string_attr(event: &MetricEvent, position: usize) -> Option<String> {
    sparse_get_string(&event.attrs, position)
        .flatten()
        .filter(|value| !value.is_empty())
}

fn value_u64(event: &MetricEvent, position: usize) -> Option<u64> {
    sparse_get_u64(&event.values, position).flatten()
}

struct CodeSummary {
    counts: CodeCounts,
    languages: BTreeMap<String, CodeCounts>,
    agents: BTreeMap<String, AgentCounts>,
    ai_files: usize,
    known_human: u64,
    unknown: u64,
}

fn summarize_files(
    files: &mut BTreeMap<String, FileCounts>,
    classifier: &Classifier,
    source_available: bool,
) -> Result<CodeSummary, String> {
    let mut counts = CodeCounts::default();
    let mut languages: BTreeMap<String, CodeCounts> = BTreeMap::new();
    let mut agents: BTreeMap<String, AgentCounts> = BTreeMap::new();
    let mut ai_files = 0;
    let mut known_human = 0;
    let mut unknown = 0;
    for (path, file) in files.iter_mut() {
        let added = range_count(&mut file.added);
        let deleted = range_count(&mut file.deleted);
        let ai = range_count(&mut file.ai);
        let human = range_count(&mut file.human);
        if ai + human > added || ranges_overlap(&file.ai, &file.human) {
            return Err("diff metadata contains overlapping attribution".into());
        }
        let test = classifier.is_test(path);
        let deleted_path = file.deleted_path.as_deref().unwrap_or(path);
        let deleted_test = classifier.is_test(deleted_path);
        counts.add(test, added, 0, ai);
        counts.add(deleted_test, 0, deleted, 0);
        languages
            .entry(classifier.language(path).into())
            .or_default()
            .add(test, added, 0, ai);
        languages
            .entry(classifier.language(deleted_path).into())
            .or_default()
            .add(deleted_test, 0, deleted, 0);
        ai_files += usize::from(ai > 0);
        known_human += human;
        unknown += added - ai - human;
        let mut source_ranges: Vec<_> = file.sources.values().flatten().copied().collect();
        let source_count = range_count(&mut source_ranges);
        if source_available {
            let add_source = |record: &mut CodeCounts| {
                let prod = record.prod_ai_surviving_source_line_num.get_or_insert(0);
                if !test {
                    *prod += source_count;
                }
                let tests = record.test_ai_surviving_source_line_num.get_or_insert(0);
                if test {
                    *tests += source_count;
                }
            };
            add_source(&mut counts);
            add_source(
                languages
                    .entry(classifier.language(path).into())
                    .or_default(),
            );
        }
        let mut agent_total = 0;
        for (agent, ranges) in &mut file.agents {
            let lines = range_count(ranges);
            agent_total += lines;
            let record = agents.entry(agent.clone()).or_insert_with(|| AgentCounts {
                agent_type: agent.clone(),
                ..Default::default()
            });
            if test {
                record.test_ai_code_line_accept_num += lines;
            } else {
                record.prod_ai_code_line_accept_num += lines;
            }
            record.changed_file_num += 1;
        }
        for (agent, ranges) in &mut file.sources {
            let lines = range_count(ranges);
            let record = agents.entry(agent.clone()).or_insert_with(|| AgentCounts {
                agent_type: agent.clone(),
                ..Default::default()
            });
            if test {
                *record.test_ai_surviving_source_line_num.get_or_insert(0) += lines;
            } else {
                *record.prod_ai_surviving_source_line_num.get_or_insert(0) += lines;
            }
            if !file.agents.contains_key(agent) {
                record.changed_file_num += 1;
            }
        }
        if agent_total != ai {
            return Err("diff metadata contains overlapping Agent attribution".into());
        }
    }
    if source_available {
        counts.prod_ai_surviving_source_line_num.get_or_insert(0);
        counts.test_ai_surviving_source_line_num.get_or_insert(0);
        for agent in agents.values_mut() {
            agent.prod_ai_surviving_source_line_num.get_or_insert(0);
            agent.test_ai_surviving_source_line_num.get_or_insert(0);
        }
    }
    Ok(CodeSummary {
        counts,
        languages,
        agents,
        ai_files,
        known_human,
        unknown,
    })
}

fn sanitized_repo(value: Option<String>) -> Option<String> {
    let normalized = crate::repo_url::normalize_repo_url(value.as_deref()?).ok()?;
    let mut url = url::Url::parse(&normalized).ok()?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    crate::repo_url::normalize_repo_url(url.as_str()).ok()
}

fn commit_record(
    event: &MetricEvent,
    username: &str,
    classifier: &Classifier,
) -> Result<CommitRecord, String> {
    let commit = string_attr(event, attr_pos::COMMIT_SHA).ok_or("commit identity is missing")?;
    let (hunks_position, note_position) = match event.event_id {
        1 => (committed_pos::HUNKS, committed_pos::AUTHORSHIP_NOTE),
        7 => (
            rewrite_committed_pos::HUNKS,
            rewrite_committed_pos::AUTHORSHIP_NOTE,
        ),
        _ => return Err("event is not a commit".into()),
    };
    let serialized_hunks = sparse_get_string(&event.values, hunks_position)
        .flatten()
        .ok_or("file-level diff metadata is unavailable")?;
    let hunks: Vec<DiffJsonHunk> =
        serde_json::from_str(&serialized_hunks).map_err(|_| "invalid file-level diff metadata")?;
    let note = sparse_get_string(&event.values, note_position)
        .flatten()
        .and_then(|value| AuthorshipLog::deserialize_from_string(&value).ok());
    let mut agent_ids = BTreeMap::new();
    if let Some(note) = note {
        for (id, record) in note.metadata.sessions {
            agent_ids.insert(
                generate_session_id(&record.agent_id.id, &record.agent_id.tool),
                record.agent_id.tool.clone(),
            );
            agent_ids.insert(id, record.agent_id.tool);
        }
        for (id, record) in note.metadata.prompts {
            agent_ids.insert(
                generate_session_id(&record.agent_id.id, &record.agent_id.tool),
                record.agent_id.tool.clone(),
            );
            agent_ids.insert(id, record.agent_id.tool);
        }
    }
    let mut files: BTreeMap<String, FileCounts> = BTreeMap::new();
    for hunk in hunks {
        let path = hunk.file_path.replace('\\', "/");
        if path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err("diff metadata contains a non-relative path".into());
        }
        if path == ".gitmodules" || path.rsplit('/').next() == Some("page.lock") {
            continue;
        }
        if hunk.commit_sha != commit || hunk.start_line == 0 || hunk.end_line < hunk.start_line {
            return Err("diff metadata has an invalid commit or line range".into());
        }
        let file = files.entry(path).or_default();
        let range = (hunk.start_line, hunk.end_line);
        match hunk.hunk_kind.as_str() {
            "deletion" => file.deleted.push(range),
            "addition" => {
                file.added.push(range);
                if hunk.session_id.is_some() || hunk.prompt_id.is_some() {
                    if hunk.human_id.is_some() {
                        return Err(
                            "diff metadata contains conflicting AI and human attribution".into(),
                        );
                    }
                    file.ai.push(range);
                    let agent = hunk
                        .prompt_id
                        .as_ref()
                        .and_then(|id| agent_ids.get(id))
                        .or_else(|| hunk.session_id.as_ref().and_then(|id| agent_ids.get(id)))
                        .cloned()
                        .unwrap_or_else(|| "unknown".into());
                    file.agents.entry(agent).or_default().push(range);
                } else if hunk.human_id.is_some() {
                    file.human.push(range);
                }
            }
            _ => return Err("diff metadata contains an unknown hunk kind".into()),
        }
    }
    let summary = summarize_files(&mut files, classifier, false)?;
    let sources: Vec<String> = if event.event_id == 7 {
        event
            .values
            .get(&rewrite_committed_pos::ORIGINAL_COMMIT_SHAS.to_string())
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|_| "invalid rewrite source metadata")
            })
            .transpose()?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let timestamp = if event.event_id == 1 {
        value_u64(event, committed_pos::COMMIT_TS)
    } else {
        None
    }
    .unwrap_or(u64::from(event.timestamp));
    let time = DateTime::<Utc>::from_timestamp(
        i64::try_from(timestamp).map_err(|_| "invalid commit time")?,
        0,
    )
    .ok_or("invalid commit time")?;
    Ok(CommitRecord {
        repo: sanitized_repo(string_attr(event, attr_pos::REPO_URL)),
        branch: string_attr(event, attr_pos::BRANCH),
        commit,
        source_commit: if sources.len() == 1 {
            sources.first().cloned()
        } else {
            None
        },
        source_commits: sources,
        user_name: username.into(),
        create_at: time.format("%Y-%m-%d %H:%M:%S").to_string(),
        timestamp: timestamp * 1000,
        timestamp_source: if event.event_id == 1
            && value_u64(event, committed_pos::COMMIT_TS).is_some()
        {
            "commit_time"
        } else {
            "recording_time"
        },
        recorded_at: u64::from(event.timestamp) * 1000,
        parent_commits: None,
        diff_coverage: "recorded_text_hunks",
        source_evidence: "unavailable",
        merge_eligibility: None,
        counts: summary.counts,
        changed_file_num: files.len(),
        ai_changed_file_num: summary.ai_files,
        known_human_line_num: summary.known_human,
        unknown_line_num: summary.unknown,
        language_stats: serde_json::to_string(&summary.languages).map_err(|e| e.to_string())?,
        ai_breakdown: summary.agents.into_values().collect(),
    })
}

fn fact_record(
    facts: &CommitFacts,
    username: &str,
    classifier: &Classifier,
) -> Result<CommitRecord, String> {
    let mut files = BTreeMap::new();
    let mut source_available = false;
    for fact in &facts.files {
        let path = &fact.path;
        let old_path = fact.previous_path.as_deref().unwrap_or(path);
        if [path.as_str(), old_path]
            .iter()
            .any(|path| path.starts_with('/') || path.split('/').any(|part| part == ".."))
        {
            return Err("local facts contain a non-relative path".into());
        }
        let eligible =
            |path: &str| path != ".gitmodules" && path.rsplit('/').next() != Some("page.lock");
        let new_eligible = eligible(path)
            && fact
                .new_mode
                .as_deref()
                .is_some_and(|mode| mode != "160000");
        let old_eligible = eligible(old_path)
            && fact
                .old_mode
                .as_deref()
                .is_some_and(|mode| mode != "160000");
        if !new_eligible && !old_eligible {
            continue;
        }
        if fact
            .added
            .iter()
            .chain(&fact.deleted)
            .any(|&(start, end)| start == 0 || end < start)
        {
            return Err("local facts contain an invalid line range".into());
        }
        for &(start, end) in fact
            .accepted
            .values()
            .flatten()
            .chain(&fact.known_human)
            .chain(
                fact.source
                    .iter()
                    .flat_map(|source| source.values().flatten()),
            )
        {
            if start == 0 || end < start {
                return Err("local facts contain an invalid attribution range".into());
            }
            let mut intersected =
                crate::authorship::enterprise::intersect_ranges(&[(start, end)], &fact.added);
            if range_count(&mut intersected) != u64::from(end) - u64::from(start) + 1 {
                return Err("local attribution is outside the committed additions".into());
            }
        }
        let mut file = FileCounts {
            deleted_path: fact.previous_path.clone(),
            ..Default::default()
        };
        if !fact.binary {
            if new_eligible {
                file.added.clone_from(&fact.added);
            }
            if old_eligible {
                file.deleted.clone_from(&fact.deleted);
            }
            if new_eligible {
                file.agents.clone_from(&fact.accepted);
                file.ai = fact.accepted.values().flatten().copied().collect();
                file.human.clone_from(&fact.known_human);
                if let Some(source) = &fact.source {
                    source_available = true;
                    file.sources.clone_from(source);
                }
            }
        }
        files.insert(path.clone(), file);
    }
    let summary = summarize_files(&mut files, classifier, source_available)?;
    if let Some(merge) = &facts.merge {
        use crate::metrics::enterprise::MergeDecision;
        let majority =
            merge.verified_ai > 0 && merge.verified_ai.saturating_mul(2) > merge.related_additions;
        let valid_decision = match merge.decision {
            MergeDecision::Eligible => facts.parent_shas.len() == 2 && majority,
            MergeDecision::NotStrictMajority => {
                facts.parent_shas.len() == 2 && merge.verified_ai > 0 && !majority
            }
            MergeDecision::MissingSource => facts.parent_shas.len() == 2 && merge.verified_ai == 0,
            MergeDecision::Octopus => facts.parent_shas.len() > 2,
            _ => facts.parent_shas.len() >= 2,
        };
        if !valid_decision
            || merge.verified_ai > merge.related_additions
            || merge.related_additions
                > summary.counts.prod_code_line_num + summary.counts.test_code_line_num
            || merge.verified_ai
                > summary
                    .counts
                    .prod_ai_surviving_source_line_num
                    .unwrap_or(0)
                    + summary
                        .counts
                        .test_ai_surviving_source_line_num
                        .unwrap_or(0)
        {
            return Err("invalid local merge evidence".into());
        }
    }
    let time = DateTime::<Utc>::from_timestamp(
        i64::try_from(facts.committed_at).map_err(|_| "invalid commit time")?,
        0,
    )
    .ok_or("invalid commit time")?;
    let mut sources = BTreeSet::new();
    for relation in &facts.relations {
        if relation.source_commits.is_empty()
            || relation
                .source_commits
                .iter()
                .any(|sha| sha.is_empty() || sha == &facts.commit_sha)
        {
            return Err("invalid local operation relation".into());
        }
        sources.extend(relation.source_commits.iter().cloned());
    }
    Ok(CommitRecord {
        repo: sanitized_repo(facts.repo_url.clone()),
        branch: None,
        commit: facts.commit_sha.clone(),
        source_commit: if sources.len() == 1 {
            sources.first().cloned()
        } else {
            None
        },
        source_commits: sources.into_iter().collect(),
        user_name: username.into(),
        create_at: time.format("%Y-%m-%d %H:%M:%S").to_string(),
        timestamp: facts
            .committed_at
            .checked_mul(1000)
            .ok_or("invalid commit time")?,
        timestamp_source: "commit_time",
        recorded_at: facts
            .recorded_at
            .checked_mul(1000)
            .ok_or("invalid recording time")?,
        parent_commits: Some(facts.parent_shas.clone()),
        diff_coverage: "complete_first_parent",
        source_evidence: if source_available {
            "partial_verified_surviving_bytes"
        } else {
            "unavailable"
        },
        merge_eligibility: facts.merge.clone(),
        counts: summary.counts,
        changed_file_num: files.len(),
        ai_changed_file_num: summary.ai_files,
        known_human_line_num: summary.known_human,
        unknown_line_num: summary.unknown,
        language_stats: serde_json::to_string(&summary.languages).map_err(|e| e.to_string())?,
        ai_breakdown: summary.agents.into_values().collect(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TokenRecord {
    record_type: &'static str,
    username: String,
    session_id: String,
    external_session_id: Option<String>,
    parent_session_id: Option<String>,
    root_session_id: String,
    root_relation_complete: bool,
    repo: Option<String>,
    agent_type: String,
    model: String,
    speed: u32,
    timestamp: u64,
    bucket_duration_seconds: u32,
    emitted_seq: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    reasoning_tokens: Option<u64>,
    total_tokens: u64,
}

type TokenKey = (String, String, u32, u64);

fn token_identity(event: &MetricEvent) -> Result<(TokenKey, u64), String> {
    let required = |position| {
        value_u64(event, position).ok_or("required Token bucket identity is unavailable")
    };
    let bucket = required(token_usage_pos::BUCKET_TS)?;
    if bucket % 300 != 0 || bucket > u64::from(u32::MAX) {
        return Err("invalid Token bucket timestamp".into());
    }
    Ok((
        (
            string_attr(event, attr_pos::SESSION_ID).ok_or("Token session identity is missing")?,
            string_attr(event, attr_pos::MODEL).ok_or("Token model is missing")?,
            u32::try_from(required(token_usage_pos::SPEED)?).map_err(|_| "invalid Token speed")?,
            bucket * 1000,
        ),
        required(token_usage_pos::EMITTED_SEQ)?,
    ))
}

fn token_record(event: &MetricEvent, username: &str) -> Result<TokenRecord, String> {
    let ((session, model, speed, timestamp), emitted_seq) = token_identity(event)?;
    let required =
        |position| value_u64(event, position).ok_or("required Token field is unavailable");
    let input = required(token_usage_pos::INPUT_TOKENS)?;
    let output = required(token_usage_pos::OUTPUT_TOKENS)?;
    let read = required(token_usage_pos::CACHE_READ_TOKENS)?;
    let write = required(token_usage_pos::CACHE_WRITE_TOKENS)?;
    let total = input
        .checked_add(output)
        .and_then(|n| n.checked_add(read))
        .and_then(|n| n.checked_add(write))
        .ok_or("Token total overflows")?;
    let reasoning = value_u64(event, token_usage_pos::REASONING_OUTPUT_TOKENS);
    if value_u64(event, token_usage_pos::TOTAL_TOKENS).is_some_and(|value| value != total)
        || reasoning.is_some_and(|value| value > output)
    {
        return Err("inconsistent Token counters".into());
    }
    Ok(TokenRecord {
        record_type: "token_bucket",
        username: username.into(),
        root_session_id: session.clone(),
        session_id: session,
        external_session_id: string_attr(event, attr_pos::EXTERNAL_SESSION_ID),
        parent_session_id: string_attr(event, attr_pos::PARENT_SESSION_ID),
        root_relation_complete: false,
        repo: sanitized_repo(string_attr(event, attr_pos::REPO_URL)),
        agent_type: string_attr(event, attr_pos::TOOL).ok_or("Token Agent identity is missing")?,
        model,
        speed,
        timestamp,
        bucket_duration_seconds: 300,
        emitted_seq,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: read,
        cache_creation_tokens: write,
        reasoning_tokens: reasoning,
        total_tokens: total,
    })
}

#[derive(Debug, Default, Serialize)]
struct RootTotal {
    username: String,
    root_session_id: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    total_tokens: u64,
    descendant_session_count: usize,
    aggregation_complete: bool,
}

fn resolve_root(
    session: &str,
    parents: &BTreeMap<String, String>,
) -> Result<(String, bool), String> {
    let mut current = session;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current) {
            return Err("cyclic parent relationships in Token history".into());
        }
        match parents.get(current) {
            Some(parent) => current = parent,
            None => return Ok((current.into(), false)),
        }
    }
}

fn observe_parent(
    parents: &mut BTreeMap<String, String>,
    session: String,
    parent: String,
) -> Result<(), String> {
    if let Some(previous) = parents.insert(session, parent.clone())
        && previous != parent
    {
        return Err("conflicting Token session parents".into());
    }
    Ok(())
}

fn same_token_revision(left: &MetricEvent, right: &MetricEvent, username: &str) -> bool {
    match (token_record(left, username), token_record(right, username)) {
        (Ok(mut left), Ok(mut right)) => {
            left.parent_session_id = None;
            right.parent_session_id = None;
            left == right
        }
        (Err(_), Err(_)) => left.values == right.values,
        _ => false,
    }
}

fn root_totals(tokens: &[TokenRecord], username: &str) -> Result<Vec<RootTotal>, String> {
    let mut roots: BTreeMap<(String, String), (RootTotal, BTreeSet<String>)> = BTreeMap::new();
    for token in tokens {
        let (total, descendants) = roots
            .entry((token.root_session_id.clone(), token.model.clone()))
            .or_insert_with(|| {
                (
                    RootTotal {
                        username: username.into(),
                        root_session_id: token.root_session_id.clone(),
                        model: token.model.clone(),
                        aggregation_complete: false,
                        ..Default::default()
                    },
                    BTreeSet::new(),
                )
            });
        for (target, value) in [
            (&mut total.input_tokens, token.input_tokens),
            (&mut total.output_tokens, token.output_tokens),
            (&mut total.cache_read_tokens, token.cache_read_tokens),
            (
                &mut total.cache_creation_tokens,
                token.cache_creation_tokens,
            ),
            (&mut total.total_tokens, token.total_tokens),
        ] {
            *target = target
                .checked_add(value)
                .ok_or("root Token total overflows")?;
        }
        if token.session_id != token.root_session_id {
            descendants.insert(token.session_id.clone());
        }
        total.descendant_session_count = descendants.len();
    }
    Ok(roots.into_values().map(|(total, _)| total).collect())
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    record_type: &'static str,
    reason: String,
}

#[derive(Debug, Serialize)]
struct Coverage {
    source: &'static str,
    moss_compatible: bool,
    limitations: Vec<&'static str>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct Snapshot {
    schema_version: &'static str,
    generator_version: &'static str,
    generated_at: String,
    username: String,
    time_zone: &'static str,
    timestamp_precision: &'static str,
    classification: Classification,
    coverage: Coverage,
    commits: Vec<CommitRecord>,
    token_usage: Vec<TokenRecord>,
    token_root_totals: Vec<RootTotal>,
}

#[derive(Clone, Copy)]
struct Window {
    since: u64,
    until: u64,
}

impl Window {
    fn parse(since: Option<&str>, until: Option<&str>) -> Result<Self, String> {
        let parse = |value: &str| {
            DateTime::parse_from_rfc3339(value)
                .map_err(|_| "time bounds must be RFC3339 timestamps".to_owned())
                .and_then(|time| {
                    if time.timestamp_subsec_nanos() != 0 {
                        return Err("time bounds must use whole seconds".to_owned());
                    }
                    u64::try_from(time.timestamp())
                        .map_err(|_| "time bounds must not precede 1970".to_owned())
                })
        };
        let window = Self {
            since: since.map(parse).transpose()?.unwrap_or(0),
            until: until.map(parse).transpose()?.unwrap_or(u64::MAX),
        };
        if window.since >= window.until {
            return Err("--since must precede --until".into());
        }
        Ok(window)
    }
    fn contains(self, seconds: u64) -> bool {
        seconds >= self.since && seconds < self.until
    }
}

#[cfg(test)]
fn snapshot(
    events: Vec<MetricEvent>,
    username: &str,
    classifier: Classifier,
    window: Window,
) -> Result<Snapshot, String> {
    snapshot_with_facts(events, Vec::new(), username, classifier, window)
}

#[cfg(test)]
fn snapshot_with_facts(
    events: Vec<MetricEvent>,
    facts: Vec<CommitFacts>,
    username: &str,
    classifier: Classifier,
    window: Window,
) -> Result<Snapshot, String> {
    snapshot_with_local_history(
        events,
        facts,
        Default::default(),
        username,
        classifier,
        window,
    )
}

fn snapshot_with_local_history(
    mut events: Vec<MetricEvent>,
    facts: Vec<CommitFacts>,
    native: crate::token_usage::native_store::NativeExport,
    username: &str,
    classifier: Classifier,
    window: Window,
) -> Result<Snapshot, String> {
    let mut commits: BTreeMap<(Option<String>, String, String), (u32, CommitRecord)> =
        BTreeMap::new();
    let facts = facts
        .into_iter()
        .map(|fact| {
            let record = fact_record(&fact, username, &classifier)?;
            Ok((fact, record))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut diagnostics = Vec::new();
    let mut replacement_targets = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let identities: BTreeSet<_> = facts
        .iter()
        .map(|(fact, _)| (fact.repository_key.clone(), fact.commit_sha.clone()))
        .collect();
    for (fact, _) in &facts {
        if fact.merge.is_some() {
            continue;
        }
        if !fact.replacement_source_available() {
            if fact
                .relations
                .iter()
                .any(|relation| relation.kind.replaces())
            {
                diagnostics.push(Diagnostic {
                    record_type: "commit",
                    reason: "replacement_target_source_unavailable".into(),
                });
            }
            continue;
        }
        for relation in &fact.relations {
            if relation.kind.replaces() {
                for source in &relation.source_commits {
                    if identities.contains(&(fact.repository_key.clone(), source.clone())) {
                        replacement_targets
                            .entry((fact.repository_key.clone(), source.clone()))
                            .or_default()
                            .insert(fact.commit_sha.clone());
                    } else {
                        diagnostics.push(Diagnostic {
                            record_type: "commit",
                            reason: "replacement_source_unavailable".into(),
                        });
                    }
                }
            }
        }
    }
    let mut superseded = BTreeSet::new();
    for key in replacement_targets.keys() {
        let mut current = key.clone();
        let mut path = BTreeSet::new();
        let valid = loop {
            if !path.insert(current.clone()) {
                break false;
            }
            let Some(targets) = replacement_targets.get(&current) else {
                break true;
            };
            if targets.len() != 1 {
                break false;
            }
            current.1 = targets.first().unwrap().clone();
        };
        if valid {
            superseded.insert(key.clone());
        } else {
            diagnostics.push(Diagnostic {
                record_type: "commit",
                reason: "replacement_conflict_or_cycle".into(),
            });
        }
    }
    let bound_events: BTreeSet<_> = facts
        .iter()
        .flat_map(|(fact, _)| fact.legacy_event_hashes.iter())
        .cloned()
        .collect();
    let mut parents = BTreeMap::new();
    for (session, parent) in native.parents {
        observe_parent(&mut parents, session, parent)?;
    }
    diagnostics.extend(native.unavailable.into_iter().map(|reason| Diagnostic {
        record_type: "token_bucket",
        reason: reason.diagnostic().into(),
    }));
    events.extend(native.events);
    for event in events.iter().filter(|event| event.event_id == 9) {
        if let (Some(session), Some(parent)) = (
            string_attr(event, attr_pos::SESSION_ID),
            string_attr(event, attr_pos::PARENT_SESSION_ID),
        ) {
            observe_parent(&mut parents, session, parent)?;
        }
    }
    for session in parents.keys() {
        resolve_root(session, &parents)?;
    }
    let mut token_events: BTreeMap<TokenKey, (u64, MetricEvent)> = BTreeMap::new();
    for event in events {
        if matches!(event.event_id, 1 | 7)
            && bound_events.contains(
                &crate::metrics::enterprise::event_hash(&event)
                    .map_err(|_| "cannot identify local metric event")?,
            )
        {
            continue;
        }
        match event.event_id {
            1 | 7 => match commit_record(&event, username, &classifier) {
                Ok(record) => {
                    let identity = string_attr(&event, attr_pos::REPO_URL).unwrap_or(
                        crate::metrics::enterprise::event_hash(&event)
                            .map_err(|_| "cannot identify local metric event")?,
                    );
                    let key = (record.repo.clone(), record.commit.clone(), identity);
                    if commits
                        .get(&key)
                        .is_none_or(|(timestamp, _)| event.timestamp >= *timestamp)
                    {
                        commits.insert(key, (event.timestamp, record));
                    }
                }
                Err(reason) => diagnostics.push(Diagnostic {
                    record_type: "commit",
                    reason,
                }),
            },
            9 => match token_identity(&event) {
                Ok((key, revision)) => {
                    if let Some((old_revision, old)) = token_events.get(&key) {
                        if revision < *old_revision {
                            continue;
                        }
                        if revision == *old_revision {
                            let same = same_token_revision(old, &event, username);
                            if !same {
                                return Err("conflicting revisions of the same Token bucket".into());
                            }
                        }
                    }
                    token_events.insert(key, (revision, event));
                }
                Err(reason) => diagnostics.push(Diagnostic {
                    record_type: "token_bucket",
                    reason,
                }),
            },
            _ => {}
        }
    }
    for (fact, record) in facts {
        if let Some(merge) = &fact.merge {
            use crate::metrics::enterprise::MergeDecision;
            if merge.decision != MergeDecision::Eligible {
                diagnostics.push(Diagnostic {
                    record_type: "commit",
                    reason: format!("merge_{}", merge.decision.as_str()),
                });
                continue;
            }
        } else if fact.parent_shas.len() > 1 {
            diagnostics.push(Diagnostic {
                record_type: "commit",
                reason: "merge_evidence_unavailable".into(),
            });
            continue;
        }
        if superseded.contains(&(fact.repository_key.clone(), fact.commit_sha.clone())) {
            continue;
        }
        commits.insert(
            (
                record.repo.clone(),
                record.commit.clone(),
                fact.repository_key,
            ),
            (u32::MAX, record),
        );
    }
    let commits = commits
        .into_iter()
        .filter_map(|(_, (_, record))| window.contains(record.timestamp / 1000).then_some(record))
        .collect();
    let tokens: Vec<_> = token_events
        .into_values()
        .filter_map(|(_, event)| match token_record(&event, username) {
            Ok(record) => Some(record),
            Err(reason) => {
                diagnostics.push(Diagnostic {
                    record_type: "token_bucket",
                    reason,
                });
                None
            }
        })
        .collect();
    let tokens = tokens
        .into_iter()
        .filter(|token| window.contains(token.timestamp / 1000))
        .map(|mut token| {
            token.parent_session_id = parents.get(&token.session_id).cloned();
            (token.root_session_id, token.root_relation_complete) =
                resolve_root(&token.session_id, &parents)?;
            Ok(token)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let totals = root_totals(&tokens, username)?;
    Ok(Snapshot {
        schema_version: "git-ai.enterprise-metrics/1",
        generator_version: env!("CARGO_PKG_VERSION"),
        generated_at: Utc::now().to_rfc3339(),
        username: username.into(),
        time_zone: "UTC",
        timestamp_precision: "seconds",
        classification: classifier.config,
        commits,
        token_usage: tokens,
        token_root_totals: totals,
        coverage: Coverage {
            source: "local_metric_history_and_opt_in_commit_facts",
            moss_compatible: false,
            diagnostics,
            limitations: vec![
                "Full AI generation ancestry is unavailable; optional verified surviving-source counts are partial lower bounds",
                "Retained local history suppresses only verified amend/rebase replacements; partial or ambiguous observations remain visible",
                "Clean two-parent imported-source eligibility is supported; unsupported merge mappings are withheld and do not establish Moss compatibility",
                "Local facts count eligible binary, rename-only, and mode-only files once; legacy events cover recorded text hunks only",
                "Token records are revised five-minute buckets, not completed turns or incremental observations",
                "Root totals cover observed local sessions; descendant completion is not established",
                "OpenCode covers native SQLite message.data usage only; session_message, cumulative-only history, auxiliary completeness, and completed turns remain unsupported",
                "Missing or expired local events cannot be reconstructed; unknown attribution is not human attribution",
            ],
        },
    })
}

fn run(options: Options) -> Result<(), String> {
    if options.username.trim().is_empty() {
        return Err("--username must not be empty".into());
    }
    let classifier = Classifier::load(options.config.as_deref())?;
    let window = Window::parse(options.since.as_deref(), options.until.as_deref())?;
    let database = options
        .database
        .map(Ok)
        .unwrap_or_else(MetricsDatabase::database_path)
        .map_err(|e| e.to_string())?;
    let (events, facts, native) = MetricsDatabase::export_snapshot(&database)
        .map_err(|e| format!("cannot read local metrics database: {e}"))?;
    let snapshot =
        snapshot_with_local_history(events, facts, native, &options.username, classifier, window)?;
    let mut bytes = serde_json::to_vec_pretty(&snapshot).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if let Some(path) = options.output {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
    } else {
        std::io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn handle_export_metrics(args: &[String]) {
    let options = Options::parse_from(
        std::iter::once("export-metrics").chain(args.iter().map(String::as_str)),
    );
    if let Err(error) = run(options) {
        eprintln!("export-metrics: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{
        CommittedValues, EventAttributes, PosEncoded, RewriteCommittedValues, TokenUsageValues,
    };
    use serde_json::json;

    fn classifier() -> Classifier {
        Classifier::load(None).unwrap()
    }

    fn token(seq: u64, input: u64, session: &str) -> MetricEvent {
        MetricEvent::new(
            &TokenUsageValues::new()
                .bucket_ts(300)
                .speed(0)
                .emitted_seq(seq)
                .input_tokens(input)
                .output_tokens(0)
                .cache_read_tokens(0)
                .cache_write_tokens(0)
                .total_tokens(input),
            EventAttributes::with_version("test")
                .session_id(session)
                .model("model")
                .tool("codex")
                .to_sparse(),
        )
    }

    fn hunk(path: &str, kind: &str, start: u32, end: u32) -> serde_json::Value {
        json!({"commit_sha":"commit", "content_hash":"hash", "hunk_kind":kind,
            "file_path":path, "start_line":start, "end_line":end})
    }

    fn committed(hunks: serde_json::Value) -> MetricEvent {
        MetricEvent::new(
            &CommittedValues::new().hunks(hunks.to_string()),
            EventAttributes::with_version("test")
                .commit_sha("commit")
                .to_sparse(),
        )
    }

    #[test]
    fn range_union_does_not_expand_large_ranges_or_double_count() {
        assert_eq!(
            range_count(&mut [(1, u32::MAX), (2, 3), (u32::MAX, u32::MAX)]),
            u64::from(u32::MAX)
        );
        assert_eq!(range_count(&mut [(5, 7), (1, 2), (2, 3), (6, 8)]), 7);
    }

    #[test]
    fn default_classification_respects_path_components_and_filename_rules() {
        let classifier = classifier();
        for path in [
            "tests/a.rs",
            "a/tests/a.rs",
            "a/__tests__/a.ts",
            "test_a.py",
            "a/a_test.go",
            "a/AThingTest.java",
            "a/a.spec.ts",
        ] {
            assert!(classifier.is_test(path), "{path}");
        }
        for path in [
            "src/contest/a.rs",
            "src/testimony.rs",
            "README.md",
            "a/specimen/main.rs",
        ] {
            assert!(!classifier.is_test(path), "{path}");
        }
        assert_eq!(classifier.language("a.RS"), "Rust");
        assert_eq!(classifier.language("header.h"), "Unknown");
    }

    #[test]
    fn duplicate_hunks_are_unioned_and_nested_gitmodules_remains_eligible() {
        let event = committed(json!([
            hunk("README.md", "addition", 1, 3),
            hunk("README.md", "addition", 2, 3),
            hunk("src/.gitmodules", "addition", 1, 1),
            hunk(".gitmodules", "addition", 1, 9),
            hunk("nested/page.lock", "addition", 1, 9),
            hunk("old.txt", "deletion", 1, 2),
        ]));
        let record = commit_record(&event, "employee", &classifier()).unwrap();
        assert_eq!(record.counts.prod_code_line_num, 4);
        assert_eq!(record.counts.prod_code_delete_line_num, 2);
        assert_eq!(record.changed_file_num, 3);
        assert_eq!(record.unknown_line_num, 4);
        assert_eq!(record.known_human_line_num, 0);
    }

    #[test]
    fn overlapping_ai_and_human_ranges_cannot_be_counted_as_disjoint() {
        let mut ai = hunk("a.rs", "addition", 1, 1);
        ai["session_id"] = json!("ai");
        let mut human = hunk("a.rs", "addition", 1, 1);
        human["human_id"] = json!("h_human");
        let event = committed(json!([hunk("a.rs", "addition", 1, 10), ai, human]));
        assert!(commit_record(&event, "employee", &classifier()).is_err());
    }

    #[test]
    fn enterprise_invalid_facts_cannot_silently_suppress_valid_legacy_events() {
        let event = committed(json!([hunk("a.rs", "addition", 1, 1)]));
        let facts = CommitFacts {
            repository_key: "repository".into(),
            repo_url: None,
            commit_sha: "commit".into(),
            parent_shas: Vec::new(),
            committed_at: 1,
            recorded_at: 2,
            legacy_event_hashes: vec![crate::metrics::enterprise::event_hash(&event).unwrap()],
            relations: Vec::new(),
            merge: None,
            files: vec![crate::metrics::enterprise::CommitFileFacts {
                path: "a.rs".into(),
                previous_path: None,
                old_mode: None,
                new_mode: Some("100644".into()),
                binary: false,
                added: vec![(1, 1)],
                deleted: Vec::new(),
                accepted: BTreeMap::new(),
                known_human: Vec::new(),
                source: Some(BTreeMap::from([("codex".into(), vec![(2, 2)])])),
                source_snapshot: None,
            }],
        };
        assert!(
            snapshot_with_facts(
                vec![event],
                vec![facts],
                "employee",
                classifier(),
                Window {
                    since: 0,
                    until: u64::MAX
                }
            )
            .is_err()
        );
    }

    #[test]
    fn unavailable_diff_data_is_diagnosed_instead_of_zero_filled() {
        let event = MetricEvent::new(
            &CommittedValues::new().git_diff_added_lines(900),
            EventAttributes::with_version("test")
                .commit_sha("missing")
                .to_sparse(),
        );
        let snapshot = snapshot(
            vec![event],
            "employee",
            classifier(),
            Window {
                since: 0,
                until: u64::MAX,
            },
        )
        .unwrap();
        assert!(snapshot.commits.is_empty());
        assert_eq!(snapshot.coverage.diagnostics.len(), 1);
    }

    #[test]
    fn legacy_source_links_alone_preserve_original() {
        let first = committed(json!([hunk("a.rs", "addition", 1, 2)]));
        let mut new_hunk = hunk("a.rs", "addition", 1, 3);
        new_hunk["commit_sha"] = json!("rewritten");
        let rewrite = MetricEvent::new(
            &RewriteCommittedValues::new()
                .hunks(json!([new_hunk]).to_string())
                .original_commit_shas(vec!["commit".into()]),
            EventAttributes::with_version("test")
                .commit_sha("rewritten")
                .to_sparse(),
        );
        let snapshot = snapshot(
            vec![first, rewrite],
            "employee",
            classifier(),
            Window {
                since: 0,
                until: u64::MAX,
            },
        )
        .unwrap();
        assert_eq!(snapshot.commits.len(), 2);
        let rewritten = snapshot
            .commits
            .iter()
            .find(|record| record.commit == "rewritten")
            .unwrap();
        assert_eq!(rewritten.source_commit.as_deref(), Some("commit"));
        assert_eq!(rewritten.counts.prod_code_line_num, 3);
    }

    #[test]
    fn malformed_latest_token_revision_must_not_resurrect_stale_usage() {
        let old = token(1, 500, "session");
        let mut latest = token(2, 30, "session");
        latest
            .values
            .remove(&token_usage_pos::INPUT_TOKENS.to_string());
        let snapshot = snapshot(
            vec![old, latest],
            "employee",
            classifier(),
            Window {
                since: 0,
                until: u64::MAX,
            },
        )
        .unwrap();
        assert!(snapshot.token_usage.is_empty());
        assert_eq!(snapshot.coverage.diagnostics.len(), 1);
    }

    #[test]
    fn zero_corrections_replace_earlier_usage() {
        let snapshot = snapshot(
            vec![token(1, 500, "session"), token(2, 0, "session")],
            "employee",
            classifier(),
            Window {
                since: 0,
                until: u64::MAX,
            },
        )
        .unwrap();
        assert_eq!(snapshot.token_usage.len(), 1);
        assert_eq!(snapshot.token_root_totals[0].total_tokens, 0);
    }

    #[test]
    fn cyclic_ancestry_and_conflicting_bucket_revisions_are_rejected() {
        let mut cycle = token(1, 5, "child");
        cycle
            .attrs
            .insert(attr_pos::PARENT_SESSION_ID.to_string(), json!("child"));
        assert!(
            snapshot(
                vec![cycle],
                "employee",
                classifier(),
                Window {
                    since: 0,
                    until: u64::MAX
                }
            )
            .is_err()
        );
        assert!(
            snapshot(
                vec![token(1, 5, "session"), token(1, 6, "session")],
                "employee",
                classifier(),
                Window {
                    since: 0,
                    until: u64::MAX
                }
            )
            .is_err()
        );
    }

    #[test]
    fn time_window_selects_bucket_start_after_reconciling_all_revisions() {
        let snapshot = snapshot(
            vec![token(1, 50, "session"), token(2, 10, "session")],
            "employee",
            classifier(),
            Window {
                since: 301,
                until: 1000,
            },
        )
        .unwrap();
        assert!(snapshot.token_usage.is_empty());
    }

    #[test]
    fn subsecond_bounds_are_rejected_instead_of_silently_widening_the_window() {
        assert!(Window::parse(Some("2026-10-01T00:00:00.500Z"), None).is_err());
        assert!(Window::parse(None, Some("2026-10-01T00:00:00.500Z")).is_err());
        assert!(Window::parse(Some("2026-10-01T00:00:00+08:00"), None).is_ok());
    }

    #[test]
    fn uppercase_extension_configuration_cannot_be_silently_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("classification.json");
        std::fs::write(&path, r#"{"language_extensions":{"RS":"MyRust"}}"#).unwrap();
        assert!(Classifier::load(Some(&path)).is_err());
    }

    fn parent_event(mut event: MetricEvent, parent: &str) -> MetricEvent {
        event
            .attrs
            .insert(attr_pos::PARENT_SESSION_ID.to_string(), json!(parent));
        event
    }

    fn token_snapshot(events: Vec<MetricEvent>, since: u64) -> Result<Snapshot, String> {
        snapshot(
            events,
            "employee",
            classifier(),
            Window {
                since,
                until: u64::MAX,
            },
        )
    }

    #[test]
    fn retained_token_parents_survive_newer_parentless_counter_revisions() {
        let result = token_snapshot(
            vec![
                parent_event(token(1, 50, "child"), "root"),
                token(2, 10, "child"),
                token(1, 20, "root"),
            ],
            0,
        )
        .unwrap();
        assert_eq!(result.token_root_totals.len(), 1);
        assert_eq!(result.token_root_totals[0].total_tokens, 30);
        assert_eq!(
            result
                .token_usage
                .iter()
                .find(|r| r.session_id == "child")
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("root")
        );
        assert!(result.token_usage.iter().all(|r| !r.root_relation_complete));
    }

    #[test]
    fn bad_token_counters_still_supply_valid_parent_evidence() {
        let mut bad = parent_event(token(1, 50, "child"), "root");
        bad.values
            .remove(&token_usage_pos::INPUT_TOKENS.to_string());
        let result =
            token_snapshot(vec![bad, token(2, 10, "child"), token(1, 20, "root")], 0).unwrap();
        assert_eq!(result.token_root_totals.len(), 1);
        assert_eq!(result.token_root_totals[0].total_tokens, 30);
    }

    #[test]
    fn invalid_selected_child_preserves_ancestry_for_valid_grandchild_without_stale_usage() {
        for replacement in [None, Some(json!("private_invalid_counter_ancestry_marker"))] {
            let mut invalid = parent_event(token(2, 700, "child"), "root");
            invalid
                .values
                .remove(&token_usage_pos::INPUT_TOKENS.to_string());
            if let Some(value) = replacement {
                invalid
                    .values
                    .insert(token_usage_pos::INPUT_TOKENS.to_string(), value);
            }
            let history = vec![
                token(1, 500, "child"),
                invalid,
                parent_event(token(1, 7, "grandchild"), "child"),
                token(1, 20, "root"),
            ];
            for events in [history.clone(), history.into_iter().rev().collect()] {
                let result = token_snapshot(events, 0).unwrap();
                assert_eq!(result.token_usage.len(), 2);
                assert!(
                    result
                        .token_usage
                        .iter()
                        .all(|row| row.session_id != "child")
                );
                let grandchild = result
                    .token_usage
                    .iter()
                    .find(|row| row.session_id == "grandchild")
                    .unwrap();
                assert_eq!(grandchild.parent_session_id.as_deref(), Some("child"));
                assert_eq!(grandchild.root_session_id, "root");
                assert!(!grandchild.root_relation_complete);
                assert_eq!(result.token_root_totals.len(), 1);
                assert_eq!(result.token_root_totals[0].total_tokens, 27);
                assert_eq!(result.token_root_totals[0].descendant_session_count, 1);
                assert_eq!(result.coverage.diagnostics.len(), 1);
                assert!(
                    !serde_json::to_string(&result)
                        .unwrap()
                        .contains("private_invalid_counter_ancestry_marker")
                );
            }
        }
    }

    #[test]
    fn token_parent_enrichment_is_independent_of_equal_counter_revision() {
        for events in [
            vec![
                token(1, 10, "child"),
                parent_event(token(1, 10, "child"), "root"),
            ],
            vec![
                parent_event(token(1, 10, "child"), "root"),
                token(1, 10, "child"),
            ],
        ] {
            let result = token_snapshot(events, 0).unwrap();
            assert_eq!(result.token_usage[0].root_session_id, "root");
            assert_eq!(
                result.token_usage[0].parent_session_id.as_deref(),
                Some("root")
            );
        }
    }

    #[test]
    fn equal_counter_revisions_cannot_hide_conflicting_nonempty_parents() {
        let history = vec![
            parent_event(token(1, 10, "child"), "root"),
            parent_event(token(1, 10, "child"), "other"),
        ];
        for events in [history.clone(), history.into_iter().rev().collect()] {
            assert!(token_snapshot(events, 0).is_err());
        }
    }

    #[test]
    fn token_parent_conflicts_and_cycles_outside_window_are_rejected() {
        assert!(
            token_snapshot(
                vec![
                    parent_event(token(1, 5, "child"), "root"),
                    parent_event(token(2, 5, "child"), "other"),
                ],
                1000
            )
            .is_err()
        );
        assert!(
            token_snapshot(
                vec![
                    parent_event(token(1, 5, "one"), "two"),
                    parent_event(token(1, 5, "two"), "one"),
                ],
                1000
            )
            .is_err()
        );
        let mut bad = parent_event(token(1, 5, "two"), "one");
        bad.values
            .remove(&token_usage_pos::INPUT_TOKENS.to_string());
        assert!(token_snapshot(vec![parent_event(token(1, 5, "one"), "two"), bad], 1000).is_err());
    }

    #[test]
    fn partial_parent_metadata_is_enriched_across_buckets() {
        let older = token(1, 5, "child");
        let mut newer = token(1, 7, "child");
        newer
            .values
            .insert(token_usage_pos::BUCKET_TS.to_string(), json!(600));
        newer
            .attrs
            .insert(attr_pos::PARENT_SESSION_ID.to_string(), json!("root"));
        let result = snapshot(
            vec![older, newer, token(1, 11, "root")],
            "employee",
            classifier(),
            Window {
                since: 0,
                until: u64::MAX,
            },
        )
        .unwrap();
        assert_eq!(result.token_root_totals.len(), 1);
        assert_eq!(result.token_root_totals[0].root_session_id, "root");
        assert_eq!(result.token_root_totals[0].input_tokens, 23);
        assert_eq!(result.token_root_totals[0].descendant_session_count, 1);
    }
}

#[cfg(test)]
mod relation_tests {
    use super::*;
    use crate::metrics::enterprise::{
        CommitFileFacts, CommitRelation, CommitRelationKind, MergeDecision, MergeFacts,
    };

    fn fact(sha: &str, time: u64) -> CommitFacts {
        CommitFacts {
            repository_key: "repo".into(),
            repo_url: None,
            commit_sha: sha.into(),
            parent_shas: Vec::new(),
            committed_at: time,
            recorded_at: time,
            legacy_event_hashes: Vec::new(),
            relations: Vec::new(),
            merge: None,
            files: vec![CommitFileFacts {
                path: "a.rs".into(),
                previous_path: None,
                old_mode: None,
                new_mode: Some("100644".into()),
                binary: false,
                added: vec![(1, 1)],
                deleted: Vec::new(),
                accepted: BTreeMap::new(),
                known_human: Vec::new(),
                source: Some(BTreeMap::new()),
                source_snapshot: None,
            }],
        }
    }

    fn replacement(sha: &str, time: u64, originals: &[&str]) -> CommitFacts {
        let mut target = fact(sha, time);
        target.relations.push(CommitRelation {
            kind: CommitRelationKind::RebaseReplacement,
            source_commits: originals.iter().map(|source| (*source).into()).collect(),
        });
        target
    }

    fn project(facts: Vec<CommitFacts>, until: u64) -> Snapshot {
        snapshot_with_facts(
            Vec::new(),
            facts,
            "employee",
            Classifier::load(None).unwrap(),
            Window { since: 0, until },
        )
        .unwrap()
    }

    fn shas(snapshot: &Snapshot) -> BTreeSet<&str> {
        snapshot
            .commits
            .iter()
            .map(|record| record.commit.as_str())
            .collect()
    }

    #[test]
    fn enterprise_replacement_chains_resolve_before_windowing() {
        assert!(
            project(
                vec![
                    fact("a", 5),
                    replacement("b", 15, &["a"]),
                    replacement("c", 25, &["b"])
                ],
                20
            )
            .commits
            .is_empty()
        );
    }

    #[test]
    fn enterprise_conflicting_and_cyclic_replacements_preserve_observations() {
        let conflict = project(
            vec![
                fact("a", 5),
                replacement("b", 15, &["a"]),
                replacement("c", 25, &["a"]),
            ],
            100,
        );
        assert_eq!(shas(&conflict), BTreeSet::from(["a", "b", "c"]));
        let cycle = project(
            vec![replacement("a", 5, &["b"]), replacement("b", 15, &["a"])],
            100,
        );
        assert_eq!(shas(&cycle), BTreeSet::from(["a", "b"]));
    }

    #[test]
    fn enterprise_partial_target_cannot_suppress_original() {
        let mut target = replacement("b", 15, &["a"]);
        target.files[0].source = None;
        assert_eq!(
            shas(&project(vec![fact("a", 5), target], 100)),
            BTreeSet::from(["a", "b"])
        );
    }

    #[test]
    fn enterprise_many_source_identity_and_repository_isolation_survive_projection() {
        let mut other = fact("a", 5);
        other.repository_key = "other-repo".into();
        let snapshot = project(
            vec![
                fact("a", 5),
                fact("b", 10),
                replacement("c", 15, &["b", "a"]),
                other,
            ],
            100,
        );
        assert_eq!(shas(&snapshot), BTreeSet::from(["a", "c"]));
        let target = snapshot
            .commits
            .iter()
            .find(|record| record.commit == "c")
            .unwrap();
        assert_eq!(target.source_commits, ["a", "b"]);
        assert!(target.source_commit.is_none());
    }

    #[test]
    fn enterprise_invalid_merge_gate_is_rejected_before_receipts() {
        let mut invalid = fact("merge", 20);
        invalid.parent_shas = vec!["p1".into(), "p2".into()];
        invalid.merge = Some(MergeFacts {
            related_additions: 1,
            verified_ai: 0,
            decision: MergeDecision::Eligible,
        });
        assert!(
            snapshot_with_facts(
                Vec::new(),
                vec![invalid],
                "employee",
                Classifier::load(None).unwrap(),
                Window {
                    since: 0,
                    until: 100
                }
            )
            .is_err()
        );
    }
}
