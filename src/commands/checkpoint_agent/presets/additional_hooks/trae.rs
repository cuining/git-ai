use super::{ParsedHookEvent, PostFileEdit, optional_str, post_file_edit, read_jsonl};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::checkpoint_transcripts::MAX_TRANSCRIPT_BYTES;
use crate::authorship::transcript::{AiTranscript, Message};
use crate::config::Config;
use crate::daemon::transcript_redaction::redact_json_secrets;
use crate::git::repo_state::worktree_root_for_path;
use crate::git::repository::{Repository, discover_repository_in_path_no_git_exec};
use crate::mdm::utils::ensure_parent_dir;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(super) fn record_message(data: &Value, event: &str, session: &str, cwd: &Path) {
    let message = match event {
        "UserPromptSubmit" => {
            optional_str(data, "prompt").map(|text| Message::user(text.to_owned(), None))
        }
        _ => optional_str(data, "last_assistant_message")
            .map(|text| Message::assistant(text.to_owned(), None)),
    };
    let Some(message) =
        message.filter(|message| message.text().is_some_and(|text| !text.is_empty()))
    else {
        return;
    };
    let roots = data["workspace_roots"].as_array().into_iter().flatten();
    let mut seen = HashSet::new();
    for path in std::iter::once(cwd).chain(roots.filter_map(Value::as_str).map(Path::new)) {
        if path.is_absolute()
            && let Ok(repo) = discover_repository_in_path_no_git_exec(path)
            && seen.insert(repo.common_dir().to_path_buf())
            && is_allowed(&repo)
        {
            let _ = record(session, &repo, message.clone(), false);
        }
    }
}

pub(super) fn repository_for_file(path: &Path) -> Option<Repository> {
    let root = worktree_root_for_path(path)?;
    let repo = discover_repository_in_path_no_git_exec(&root).ok()?;
    (repo.path_is_in_workdir(path) && is_allowed(&repo)).then_some(repo)
}

fn is_allowed(repo: &Repository) -> bool {
    let config = Config::get();
    !config.has_repository_filters()
        || config.is_allowed_repository_with_remotes(repo.remotes_with_urls().ok().as_ref())
}

pub(super) fn tool_checkpoints(
    edit: PostFileEdit,
    data: &Value,
    tool: &str,
) -> Vec<ParsedHookEvent> {
    let mut by_repo: HashMap<PathBuf, (Repository, Vec<PathBuf>)> = HashMap::new();
    for path in &edit.file_paths {
        if let Some(repo) = repository_for_file(path) {
            by_repo
                .entry(repo.common_dir().to_path_buf())
                .or_insert_with(|| (repo, Vec::new()))
                .1
                .push(path.clone());
        }
    }
    by_repo
        .into_values()
        .map(|(repo, paths)| {
            // A tool call spanning repositories can contain other roots' paths and edits.
            let input = if paths.len() == edit.file_paths.len() {
                data["tool_input"].clone()
            } else {
                json!({"file_paths": paths})
            };
            let message = Message::tool_use(tool.to_owned(), input);
            let transcript =
                record(&edit.context.external_session_id, &repo, message, true).unwrap_or_default();
            post_file_edit(
                PostFileEdit {
                    file_paths: paths,
                    ..edit.clone()
                },
                transcript,
            )
        })
        .collect()
}

fn record(
    session: &str,
    repo: &Repository,
    message: Message,
    snapshot: bool,
) -> io::Result<AiTranscript> {
    let session = generate_session_id(session, "trae");
    let path = repo
        .common_dir()
        .join("ai/trae-transcripts")
        .join(format!("{session}.jsonl"));
    ensure_parent_dir(&path).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true).read(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.lock()?;
    let mut bytes = serde_json::to_vec(&redact_json_secrets(serde_json::to_value(message)?))?;
    if bytes.len() <= MAX_TRANSCRIPT_BYTES {
        use std::io::{Seek, SeekFrom};

        file.seek(SeekFrom::End(0))?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
    }
    let mut transcript = AiTranscript::new();
    if snapshot {
        transcript.messages = read_jsonl(&mut file)?
            .filter_map(|row| serde_json::from_value(row).ok())
            .collect();
    }
    Ok(transcript)
}
