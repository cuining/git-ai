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
