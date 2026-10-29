use crate::authorship::attribution_tracker::{Attribution, AttributionTracker};
use crate::authorship::working_log::AgentId;
use crate::error::GitAiError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiSourceSnapshot {
    pub blob_sha: String,
    pub attributions: Vec<Attribution>,
    pub agents: BTreeMap<String, AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<u128>,
}

impl AiSourceSnapshot {
    pub fn capture(
        previous: Option<&Self>,
        before: &str,
        after: &str,
        author: &str,
        agent: Option<&AgentId>,
        ts: u128,
    ) -> Result<Self, GitAiError> {
        let previous = previous.filter(|snapshot| snapshot.matches(before));
        let tracker = AttributionTracker::new();
        let filled = tracker.attribute_unattributed_ranges(
            before,
            previous.map_or(&[], |snapshot| snapshot.attributions.as_slice()),
            "human",
            ts.saturating_sub(1),
        );
        let mut attributions = tracker.update_attributions_for_checkpoint(
            before,
            after,
            &filled,
            author,
            ts,
            agent.is_some(),
        )?;
        let mut agents = previous.map_or_else(BTreeMap::new, |snapshot| snapshot.agents.clone());
        if let Some(agent) = agent.filter(|agent| {
            author.len() <= 1024
                && !agent.tool.is_empty()
                && agent.tool.len() <= 128
                && !agent.id.is_empty()
                && agent.id.len() <= 512
                && agent.model.len() <= 256
                && [&agent.tool, &agent.id, &agent.model]
                    .iter()
                    .all(|value| !value.chars().any(char::is_control))
        }) {
            agents.insert(author.to_owned(), agent.clone());
        }
        attributions.retain(|attr| attr.start < attr.end && agents.contains_key(&attr.author_id));
        agents.retain(|id, _| attributions.iter().any(|attr| &attr.author_id == id));
        Ok(Self {
            blob_sha: Self::content_hash(after),
            attributions,
            agents,
            captured_at: Some(ts),
        })
    }

    pub fn project(&self, before: &str, after: &str) -> Result<Self, GitAiError> {
        let mut projected = Self::capture(Some(self), before, after, "human", None, u128::MAX - 1)?;
        projected.captured_at = None;
        Ok(projected)
    }

    pub fn newly_captured(&self) -> Self {
        let mut source = self.clone();
        source
            .attributions
            .retain(|attr| Some(attr.ts) == self.captured_at);
        source
            .agents
            .retain(|id, _| source.attributions.iter().any(|attr| &attr.author_id == id));
        source
    }

    pub fn overlay(&self, newer: &Self) -> Result<Self, GitAiError> {
        if self.blob_sha != newer.blob_sha {
            return Err(GitAiError::Generic(
                "source snapshots have different coordinates".into(),
            ));
        }
        let mut source = self.clone();
        for replacement in &newer.attributions {
            source.attributions = source
                .attributions
                .into_iter()
                .flat_map(|attr| {
                    if attr.end <= replacement.start || attr.start >= replacement.end {
                        return vec![attr];
                    }
                    let mut remaining = Vec::new();
                    if attr.start < replacement.start {
                        remaining.push(Attribution {
                            end: replacement.start,
                            ..attr.clone()
                        });
                    }
                    if attr.end > replacement.end {
                        remaining.push(Attribution {
                            start: replacement.end,
                            ..attr
                        });
                    }
                    remaining
                })
                .collect();
        }
        source
            .attributions
            .extend(newer.attributions.iter().cloned());
        source.attributions.sort_by(|left, right| {
            (left.start, left.end, &left.author_id, left.ts).cmp(&(
                right.start,
                right.end,
                &right.author_id,
                right.ts,
            ))
        });
        source.agents.extend(newer.agents.clone());
        source
            .agents
            .retain(|id, _| source.attributions.iter().any(|attr| &attr.author_id == id));
        source.captured_at = None;
        Ok(source)
    }

    pub fn matches(&self, content: &str) -> bool {
        self.blob_sha == Self::content_hash(content)
            && self.attributions.iter().all(|attr| {
                attr.start < attr.end
                    && attr.end <= content.len()
                    && self.agents.contains_key(&attr.author_id)
            })
    }

    fn content_hash(content: &str) -> String {
        format!("{:x}", Sha256::digest(content.as_bytes()))
    }
}
