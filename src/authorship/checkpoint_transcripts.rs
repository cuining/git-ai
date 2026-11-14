//! Local transcript snapshots addressed by the existing session and checkpoint IDs in Git Notes.

use crate::authorship::authorship_log_serialization::generate_trace_id;
use crate::authorship::transcript::AiTranscript;
use crate::daemon::transcript_redaction::redact_json_secrets;
use crate::error::GitAiError;
use crate::git::repository::Repository;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

pub(crate) const MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;

fn snapshot_path(repo: &Repository, session_id: &str, trace_id: &str) -> Option<PathBuf> {
    fn valid(id: &str, prefix: &str) -> bool {
        id.strip_prefix(prefix).is_some_and(|hex| {
            hex.len() == 14
                && hex
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
    }
    if !valid(session_id, "s_") || !valid(trace_id, "t_") {
        return None;
    }
    Some(
        repo.common_dir()
            .join("ai")
            .join("checkpoint-transcripts")
            .join(session_id)
            .join(format!("{trace_id}.json")),
    )
}

pub(crate) fn save(
    repo: &Repository,
    session_id: &str,
    trace_id: &str,
    transcript: &AiTranscript,
) -> Result<(), GitAiError> {
    let path = snapshot_path(repo, session_id, trace_id)
        .ok_or_else(|| GitAiError::Generic("Invalid checkpoint transcript key".into()))?;
    if path.try_exists()? {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&redact_json_secrets(serde_json::to_value(transcript)?))?;
    if bytes.len() > MAX_TRANSCRIPT_BYTES {
        return Err(GitAiError::Generic(
            "Checkpoint transcript exceeds size limit".into(),
        ));
    }

    crate::mdm::utils::ensure_parent_dir(&path)?;
    let temp = path.with_extension(format!("{}.tmp", generate_trace_id()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(&temp)?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result.and_then(|()| fs::rename(&temp, &path)) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

pub(crate) fn load(
    repo: &Repository,
    attestation: &str,
) -> Result<Option<AiTranscript>, GitAiError> {
    let Some(path) = attestation
        .split_once("::")
        .and_then(|(session_id, trace_id)| snapshot_path(repo, session_id, trace_id))
    else {
        return Ok(None);
    };
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
