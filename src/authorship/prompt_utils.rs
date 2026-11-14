use crate::authorship::authorship_log::{PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::error::GitAiError;
use crate::git::notes_api::{read_authorship, read_notes_batch, search_notes};
use crate::git::repository::Repository;

/// Find a prompt in the repository history
///
/// If `commit` is provided, look only in that specific commit.
/// Otherwise, search through history and skip `offset` occurrences (0 = most recent).
pub fn find_prompt(
    repo: &Repository,
    prompt_id: &str,
    commit: Option<&str>,
    offset: usize,
) -> Result<(String, PromptRecord), GitAiError> {
    if let Some(commit_rev) = commit {
        // Look in specific commit
        find_prompt_in_commit(repo, prompt_id, commit_rev)
    } else {
        // Search through history with offset
        find_prompt_in_history(repo, prompt_id, offset)
    }
}

/// Find a prompt in a specific commit (searches both prompts and sessions)
pub fn find_prompt_in_commit(
    repo: &Repository,
    prompt_id: &str,
    commit_rev: &str,
) -> Result<(String, PromptRecord), GitAiError> {
    // Resolve the revision to a commit SHA
    let commit = repo.revparse_single(commit_rev)?;
    let commit_sha = commit.id();

    // Get the authorship log for this commit
    let authorship_log = read_authorship(repo, &commit_sha).ok_or_else(|| {
        GitAiError::Generic(format!(
            "No authorship data found for commit: {}",
            commit_rev
        ))
    })?;

    // Look for the prompt in the prompts map first
    if let Some(prompt) = authorship_log.metadata.prompts.get(prompt_id) {
        return Ok((commit_sha, prompt.clone()));
    }

    if let Some(session) = find_attested_session(&authorship_log, prompt_id) {
        return Ok((commit_sha, session.to_prompt_record()));
    }

    Err(GitAiError::Generic(format!(
        "Prompt '{}' not found in commit {}",
        prompt_id, commit_rev
    )))
}

/// Find a prompt in history, skipping `offset` occurrences
/// Returns the (N+1)th occurrence where N = offset (0 = most recent)
pub fn find_prompt_in_history(
    repo: &Repository,
    prompt_id: &str,
    offset: usize,
) -> Result<(String, PromptRecord), GitAiError> {
    // Session IDs and legacy prompt IDs are JSON keys. Full checkpoint
    // attestations appear in the line ranges above the JSON metadata.
    let pattern = if prompt_id.starts_with("s_") && prompt_id.contains("::") {
        prompt_id.to_owned()
    } else {
        format!("\"{prompt_id}\"")
    };
    let shas = search_notes(repo, &pattern).unwrap_or_default();

    if shas.is_empty() {
        return Err(GitAiError::Generic(format!(
            "Prompt not found in history: {}",
            prompt_id
        )));
    }

    let notes = read_notes_batch(repo, &shas)?;
    let mut found_count = 0;
    for sha in &shas {
        let Some(log) = notes
            .get(sha)
            .and_then(|content| AuthorshipLog::deserialize_from_string(content).ok())
        else {
            continue;
        };
        let prompt = log.metadata.prompts.get(prompt_id).cloned().or_else(|| {
            find_attested_session(&log, prompt_id).map(SessionRecord::to_prompt_record)
        });
        if let Some(prompt) = prompt {
            if found_count == offset {
                return Ok((sha.clone(), prompt));
            }
            found_count += 1;
        }
    }

    // If we get here, we didn't find enough occurrences
    if found_count == 0 {
        Err(GitAiError::Generic(format!(
            "Prompt not found in history: {}",
            prompt_id
        )))
    } else {
        Err(GitAiError::Generic(format!(
            "Prompt '{}' found {} time(s), but offset {} requested (max offset: {})",
            prompt_id,
            found_count,
            offset,
            found_count - 1
        )))
    }
}

fn find_attested_session<'a>(log: &'a AuthorshipLog, id: &str) -> Option<&'a SessionRecord> {
    if id.starts_with("s_")
        && let Some((session_id, _)) = id.split_once("::")
    {
        let attested = log
            .attestations
            .iter()
            .flat_map(|file| &file.entries)
            .any(|entry| entry.hash == id);
        if !attested {
            return None;
        }
        return log.metadata.sessions.get(session_id);
    }
    log.metadata.sessions.get(id)
}
