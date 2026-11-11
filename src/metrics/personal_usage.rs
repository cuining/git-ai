//! Cross-repository committed contributions from local ID 1 and ID 7 events.

use crate::error::GitAiError;
use crate::git::repo_state::is_valid_git_oid;
use crate::metrics::attrs::attr_pos;
use crate::metrics::db::{MetricHistoryRecord, MetricsDatabase};
use crate::metrics::events::{committed_pos, rewrite_committed_pos};
use crate::metrics::pos_encoded::{sparse_get_string, sparse_get_u32, sparse_get_vec_u32};
use crate::metrics::types::{MetricEvent, MetricEventId};
use chrono::{Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

type CommitKey = (String, String);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct Totals {
    commit_count: u64,
    added_lines: u64,
    ai_lines: u64,
    human_lines: u64,
    unknown_lines: u64,
}

impl Totals {
    fn from_commit(event: &MetricEvent) -> Option<Self> {
        let added = sparse_get_u32(&event.values, committed_pos::GIT_DIFF_ADDED_LINES).flatten()?;
        let human = sparse_get_u32(&event.values, committed_pos::HUMAN_ADDITIONS)
            .flatten()
            .unwrap_or(0);
        let ai = sparse_get_vec_u32(&event.values, committed_pos::AI_ADDITIONS)
            .flatten()
            .and_then(|lines| lines.first().copied())
            .unwrap_or(0);
        let unknown = added.checked_sub(ai)?.checked_sub(human)?;
        Some(Self {
            commit_count: 1,
            added_lines: u64::from(added),
            ai_lines: u64::from(ai),
            human_lines: u64::from(human),
            unknown_lines: u64::from(unknown),
        })
    }

    fn accumulate(&mut self, commit: Self) {
        self.commit_count += commit.commit_count;
        self.added_lines += commit.added_lines;
        self.ai_lines += commit.ai_lines;
        self.human_lines += commit.human_lines;
        self.unknown_lines += commit.unknown_lines;
    }
}

#[derive(Serialize)]
struct Day {
    date: NaiveDate,
    #[serde(flatten)]
    totals: Totals,
}

#[derive(Serialize)]
struct RepositoryUsage {
    repo: String,
    #[serde(flatten)]
    totals: Totals,
}

#[derive(Serialize)]
pub(crate) struct PersonalUsage {
    totals: Totals,
    daily: Vec<Day>,
    repos: Vec<RepositoryUsage>,
    pending_events: usize,
}

fn local_date(ts: u32) -> Option<NaiveDate> {
    Local
        .timestamp_opt(i64::from(ts), 0)
        .single()
        .map(|dt| dt.date_naive())
}

struct Commit {
    original_ts: u32,
    totals: Option<Totals>,
    conflict: bool,
    relevant: bool,
}

#[derive(Default)]
struct Link {
    sources: Option<Vec<CommitKey>>,
    conflict: bool,
    relevant: bool,
}

#[derive(Deserialize)]
struct Rewrite {
    v: u8,
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    before_sha: String,
    after_sha: String,
    complete: bool,
    amend_same_tree: Option<bool>,
    replaced_shas: Vec<String>,
    targets: Vec<RewriteTarget>,
}

#[derive(Deserialize)]
struct RewriteTarget {
    sha: String,
    sources: Vec<String>,
}

impl Rewrite {
    fn has_valid_mapping(&self, head: &str) -> bool {
        if self.v != 1
            || !self.complete
            || self.after_sha != head
            || !is_valid_git_oid(&self.before_sha)
            || self
                .reference
                .strip_prefix("refs/heads/")
                .is_none_or(str::is_empty)
        {
            return false;
        }
        let targets: HashSet<&str> = self
            .targets
            .iter()
            .map(|target| target.sha.as_str())
            .collect();
        let sources: HashSet<&str> = self
            .targets
            .iter()
            .flat_map(|target| target.sources.iter().map(String::as_str))
            .collect();
        if !targets.contains(head)
            || targets.len() != self.targets.len()
            || sources.is_empty()
            || !sources.is_disjoint(&targets)
            || !sources
                .iter()
                .chain(targets.iter())
                .all(|sha| is_valid_git_oid(sha))
        {
            return false;
        }
        match self.kind.as_str() {
            "rebase" => {
                let replaced: HashSet<&str> =
                    self.replaced_shas.iter().map(String::as_str).collect();
                self.targets.iter().all(|target| target.sources.len() == 1)
                    && sources.len() == targets.len()
                    && replaced == sources
                    && replaced.len() == self.replaced_shas.len()
                    && replaced.contains(self.before_sha.as_str())
            }
            "amend" => {
                self.amend_same_tree == Some(true)
                    && self.targets.len() == 1
                    && self.replaced_shas == [self.before_sha.as_str()]
                    && self.targets[0].sources == [self.before_sha.as_str()]
            }
            "squash_merge" => {
                self.replaced_shas.is_empty()
                    && self.targets.len() == 1
                    && sources.len() == self.targets[0].sources.len()
            }
            _ => false,
        }
    }
}

fn commit_key(record: &MetricHistoryRecord) -> Option<CommitKey> {
    let attrs = &record.event.attrs;
    let sha = sparse_get_string(attrs, attr_pos::COMMIT_SHA).flatten()?;
    if !is_valid_git_oid(&sha) {
        return None;
    }
    let repo = record
        .repo_url
        .as_deref()
        .and_then(|remote| crate::repo_url::normalize_repo_url(remote).ok())
        .or_else(|| {
            sparse_get_string(attrs, attr_pos::LOCAL_REPO_ID)
                .flatten()
                .filter(|id| !id.is_empty())
                .map(|id| format!("local:{id}"))
        })?;
    Some((repo, sha.to_ascii_lowercase()))
}

fn record_link(
    links: &mut HashMap<CommitKey, Link>,
    key: CommitKey,
    relevant: bool,
    sources: Option<Vec<CommitKey>>,
) {
    let link = links.entry(key).or_default();
    link.relevant |= relevant;
    if let Some(sources) = sources {
        if let Some(existing) = &link.sources {
            link.conflict |= *existing != sources;
        } else {
            link.sources = Some(sources);
        }
    }
}

fn record_rewrite(
    links: &mut HashMap<CommitKey, Link>,
    key: CommitKey,
    event: &MetricEvent,
    relevant: bool,
) {
    let result = sparse_get_string(&event.values, rewrite_committed_pos::REWRITE_RESULT)
        .flatten()
        .and_then(|raw| serde_json::from_str::<Rewrite>(&raw).ok());
    let Some(result) = result.filter(|result| result.after_sha == key.1) else {
        record_link(links, key, relevant, None);
        return;
    };
    let valid = result.has_valid_mapping(&key.1);
    for target in result.targets {
        if is_valid_git_oid(&target.sha) {
            let sources = valid.then(|| {
                let mut sources: Vec<_> = target
                    .sources
                    .into_iter()
                    .map(|sha| (key.0.clone(), sha.to_ascii_lowercase()))
                    .collect();
                sources.sort_unstable();
                sources
            });
            record_link(
                links,
                (key.0.clone(), target.sha.to_ascii_lowercase()),
                relevant,
                sources,
            );
        }
    }
    if !valid {
        record_link(links, key, relevant, None);
    }
}

// Resolve derivative versions only once every source has an original ID 1
// behind it. Missing history and cycles therefore remain pending.
fn anchored_commits(
    commits: &HashMap<CommitKey, Commit>,
    links: &HashMap<CommitKey, Link>,
) -> HashSet<CommitKey> {
    let mut anchored: HashSet<_> = commits
        .iter()
        .filter(|(key, commit)| {
            !links.contains_key(*key) && !commit.conflict && commit.totals.is_some()
        })
        .map(|(key, _)| key.clone())
        .collect();
    let mut queue: Vec<_> = anchored.iter().cloned().collect();
    let mut children: HashMap<&CommitKey, Vec<&CommitKey>> = HashMap::new();
    let mut remaining = HashMap::new();
    for (target, link) in links {
        if let Some(sources) = &link.sources
            && !link.conflict
        {
            remaining.insert(target, sources.len());
            for source in sources {
                children.entry(source).or_default().push(target);
            }
        }
    }
    while let Some(source) = queue.pop() {
        if let Some(targets) = children.remove(&source) {
            for target in targets {
                let count = remaining.get_mut(target).expect("known rewrite target");
                *count -= 1;
                if *count == 0 && anchored.insert(target.clone()) {
                    queue.push(target.clone());
                }
            }
        }
    }
    anchored
}

pub(crate) fn query(start: NaiveDate, end: NaiveDate) -> Result<PersonalUsage, GitAiError> {
    let history = MetricsDatabase::global()?
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".into()))?
        .get_metric_history(
            0,
            None,
            &[
                MetricEventId::Committed as u16,
                MetricEventId::RewriteCommitted as u16,
            ],
        )?;
    let mut commits: HashMap<CommitKey, Commit> = HashMap::new();
    let mut links = HashMap::new();
    let mut result = PersonalUsage {
        totals: Totals::default(),
        daily: Vec::new(),
        repos: Vec::new(),
        pending_events: 0,
    };
    let in_window = |date: &NaiveDate| (start..=end).contains(date);
    for record in history {
        let relevant = local_date(record.ts).as_ref().is_some_and(in_window);
        let Some(key) = commit_key(&record) else {
            result.pending_events += usize::from(relevant);
            continue;
        };
        if record.event_id == MetricEventId::RewriteCommitted as u16 {
            record_rewrite(&mut links, key, &record.event, relevant);
            continue;
        }
        let totals = Totals::from_commit(&record.event);
        match commits.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Commit {
                    original_ts: record.ts,
                    totals,
                    conflict: false,
                    relevant,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let commit = entry.get_mut();
                commit.relevant |= relevant;
                commit.conflict |= commit.totals != totals;
                commit.original_ts = commit.original_ts.min(record.ts);
            }
        }
    }
    let anchored = anchored_commits(&commits, &links);
    for (target, link) in &links {
        let relevant = link.relevant || commits.get(target).is_some_and(|commit| commit.relevant);
        if !anchored.contains(target) && relevant {
            result.pending_events += 1;
        }
    }
    let mut daily: BTreeMap<NaiveDate, Totals> = BTreeMap::new();
    let mut repos: BTreeMap<String, Totals> = BTreeMap::new();
    for (key, commit) in commits {
        if links.contains_key(&key) {
            continue;
        }
        if commit.conflict {
            result.pending_events += usize::from(commit.relevant);
            continue;
        }
        let Some(date) = local_date(commit.original_ts).filter(in_window) else {
            continue;
        };
        if let Some(totals) = commit.totals {
            daily.entry(date).or_default().accumulate(totals);
            repos.entry(key.0).or_default().accumulate(totals);
            result.totals.accumulate(totals);
        } else {
            result.pending_events += 1;
        }
    }
    result.daily = daily
        .into_iter()
        .map(|(date, totals)| Day { date, totals })
        .collect();
    result.repos = repos
        .into_iter()
        .map(|(repo, totals)| RepositoryUsage { repo, totals })
        .collect();
    Ok(result)
}
