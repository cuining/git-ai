use crate::authorship::source_evidence::AiSourceSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type InclusiveLineRange = (u32, u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFileFacts {
    pub path: String,
    pub previous_path: Option<String>,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    pub binary: bool,
    pub added: Vec<InclusiveLineRange>,
    pub deleted: Vec<InclusiveLineRange>,
    pub accepted: BTreeMap<String, Vec<InclusiveLineRange>>,
    pub known_human: Vec<InclusiveLineRange>,
    pub source: Option<BTreeMap<String, Vec<InclusiveLineRange>>>,
    pub source_snapshot: Option<AiSourceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFacts {
    pub repository_key: String,
    pub repo_url: Option<String>,
    pub commit_sha: String,
    pub parent_shas: Vec<String>,
    pub committed_at: u64,
    pub recorded_at: u64,
    pub legacy_event_hashes: Vec<String>,
    pub files: Vec<CommitFileFacts>,
    #[serde(default)]
    pub relations: Vec<CommitRelation>,
    #[serde(default)]
    pub merge: Option<MergeFacts>,
}

impl CommitFacts {
    pub(crate) fn replacement_source_available(&self) -> bool {
        self.files.iter().all(|file| {
            file.added.is_empty()
                || file.binary
                || file.new_mode.as_deref() == Some("160000")
                || file.path == ".gitmodules"
                || file.path.rsplit('/').next() == Some("page.lock")
                || file.source.is_some()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRelation {
    pub kind: CommitRelationKind,
    pub source_commits: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitRelationKind {
    AmendReplacement,
    RebaseReplacement,
    Amend,
    Rebase,
    CherryPick,
    CherryPickNoCommit,
    Revert,
    SquashMerge,
    UpdateRef,
    NonFastForward,
}

impl CommitRelationKind {
    pub(crate) fn replaces(self) -> bool {
        matches!(self, Self::AmendReplacement | Self::RebaseReplacement)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeFacts {
    pub related_additions: u64,
    pub verified_ai: u64,
    pub decision: MergeDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeDecision {
    Eligible,
    NotStrictMajority,
    MissingSource,
    ConflictOrManualResolution,
    Octopus,
    RepeatedOrAmbiguousImport,
    UnsupportedMapping,
}

impl MergeDecision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::NotStrictMajority => "not_strict_majority",
            Self::MissingSource => "missing_source",
            Self::ConflictOrManualResolution => "conflict_or_manual_resolution",
            Self::Octopus => "octopus",
            Self::RepeatedOrAmbiguousImport => "repeated_or_ambiguous_import",
            Self::UnsupportedMapping => "unsupported_mapping",
        }
    }
}

pub(crate) fn event_hash(event: &crate::metrics::MetricEvent) -> Result<String, serde_json::Error> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json_canonicalizer::to_vec(event)?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{CommittedValues, EventAttributes, MetricEvent, PosEncoded};

    #[test]
    fn original_local_facts_without_operation_fields_still_deserialize() {
        let fact: CommitFacts = serde_json::from_value(serde_json::json!({
            "repository_key":"repo", "repo_url":null, "commit_sha":"old", "parent_shas":[],
            "committed_at":1, "recorded_at":2, "legacy_event_hashes":[], "files":[]
        }))
        .unwrap();
        assert!(fact.relations.is_empty());
        assert!(fact.merge.is_none());
    }

    #[test]
    fn enterprise_event_fingerprint_survives_persistence_roundtrip() {
        let event = MetricEvent::new(
            &CommittedValues::new()
                .human_additions(1)
                .git_diff_added_lines(3)
                .git_diff_deleted_lines(2),
            EventAttributes::with_version("test")
                .commit_sha("commit")
                .tool("codex")
                .session_id("session")
                .to_sparse(),
        );
        let expected = event_hash(&event).unwrap();
        for _ in 0..10 {
            let restored: MetricEvent =
                serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
            assert_eq!(expected, event_hash(&restored).unwrap());
        }
    }
}
