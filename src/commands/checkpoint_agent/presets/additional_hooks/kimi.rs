use super::{
    ParsedHookEvent, PostFileEdit, PresetContext, add_text, add_tool, agent_home, failed, jsonl,
    limit_transcript, optional_str, post_file_edit,
};
use crate::authorship::authorship_log_serialization::generate_trace_id;
use crate::authorship::transcript::AiTranscript;
use crate::config::Config;
use crate::git::repo_state::worktree_root_for_path;
use crate::git::repository::discover_repository_in_path_no_git_exec;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn home() -> PathBuf {
    agent_home("KIMI_CODE_HOME", ".kimi-code")
}

pub(super) fn read_session(
    session: &str,
    model: &mut Option<String>,
    transcript: &mut AiTranscript,
) -> bool {
    read_wire(session, model, transcript, None)
}

pub(super) enum WriteHook<'a> {
    Pre(&'a [PathBuf]),
    Post(&'a str),
    Stop,
}

pub(super) fn write_checkpoints(
    mut context: PresetContext,
    hook: WriteHook<'_>,
) -> Option<Vec<ParsedHookEvent>> {
    let mut writes = Writes {
        post_call: match hook {
            WriteHook::Post(id) => Some(id),
            _ => None,
        },
        pending: HashMap::new(),
        completed: Vec::new(),
    };
    let mut transcript = AiTranscript::new();
    let mut model = Some(context.agent_id.model.clone());
    if !read_wire(
        &context.external_session_id,
        &mut model,
        &mut transcript,
        Some(&mut writes),
    ) {
        return None;
    }
    if let Some(model) = model {
        context.agent_id.model = model;
    }
    limit_transcript(&mut transcript);
    // Stop can observe the same Write as PostToolUse. Both submit the recorded
    // content and call ID, and the daemon consumes the matching Pre once.
    let config = Config::get();
    Some(
        writes
            .completed
            .into_iter()
            .filter(|(_, write)| {
                // Flush an earlier completed Write before a new Pre replaces its baseline.
                if let WriteHook::Pre(paths) = hook
                    && !paths.iter().any(|path| same_path(path, &write.path))
                {
                    return false;
                }
                worktree_root_for_path(&write.path).is_some_and(|root| {
                    !config.has_repository_filters()
                        || config.is_allowed_repository_with_remotes(
                            discover_repository_in_path_no_git_exec(&root)
                                .ok()
                                .and_then(|repo| repo.remotes_with_urls().ok())
                                .as_ref(),
                        )
                })
            })
            .enumerate()
            .map(|(index, (id, write))| {
                let mut context = context.clone();
                if index > 0 {
                    context.trace_id = generate_trace_id();
                }
                if write.append {
                    context
                        .metadata
                        .insert("kimi_write_mode".into(), "append".into());
                }
                post_file_edit(
                    PostFileEdit {
                        context,
                        file_paths: vec![write.path.clone()],
                        dirty_files: Some(HashMap::from([(write.path, write.content)])),
                        stream_source: None,
                        tool_use_id: Some(id),
                    },
                    transcript.clone(),
                )
            })
            .collect(),
    )
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize()
            .is_ok_and(|path| b.canonicalize().is_ok_and(|other| path == other))
}

fn wire_path(session: &str) -> Option<PathBuf> {
    let home = home();
    let session_dir = jsonl(&home.join("session_index.jsonl"))
        .filter(|row| row["sessionId"] == session)
        .filter_map(|row| optional_str(&row, "sessionDir").map(PathBuf::from))
        .last();
    session_dir
        .filter(|path| {
            path.starts_with(home.join("sessions"))
                && path.file_name().is_some_and(|name| name == session)
                && !path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
        })
        .map(|path| path.join("agents/main/wire.jsonl"))
}

fn read_wire(
    session: &str,
    model: &mut Option<String>,
    transcript: &mut AiTranscript,
    mut writes: Option<&mut Writes<'_>>,
) -> bool {
    let Some(path) = wire_path(session) else {
        return false;
    };
    let mut local = false;
    for row in jsonl(&path) {
        let kind = row["type"].as_str();
        let binding = match kind {
            Some("runtime.set_binding") => Some(&row["runtimeId"]),
            Some("environment.set_binding") => Some(&row["environmentId"]),
            _ => None,
        };
        if let Some(binding) = binding {
            local = binding == "local";
            if let Some(writes) = writes.as_mut() {
                writes.pending.clear();
                writes.completed.clear();
            }
            if !local {
                transcript.messages.clear();
            }
        }
        if !local {
            continue;
        }
        match kind {
            Some("turn.prompt" | "turn.ended") => {
                if let Some(writes) = writes.as_mut().filter(|writes| writes.post_call.is_none()) {
                    writes.pending.clear();
                    writes.completed.clear();
                }
            }
            Some("llm.request") => {
                if let Some(value) = optional_str(&row, "model") {
                    *model = Some(value.to_owned());
                }
            }
            Some("context.append_message") => {
                let message = &row["message"];
                if message["role"] == "user" && message["origin"]["kind"] == "user" {
                    add_text(
                        transcript,
                        "user",
                        &message["content"],
                        optional_str(&row, "time"),
                    );
                }
            }
            Some("context.append_loop_event") => {
                let event = &row["event"];
                if let Some(writes) = writes.as_mut() {
                    writes.observe(event);
                }
                let timestamp = optional_str(&row, "time");
                match optional_str(event, "type") {
                    Some("content.part") => {
                        add_text(transcript, "assistant", &event["part"], timestamp)
                    }
                    Some("tool.call") => add_tool(transcript, event, "args", timestamp),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    local
}

struct Write {
    path: PathBuf,
    content: String,
    append: bool,
}

impl Write {
    fn from_call(event: &Value) -> Option<Self> {
        let path = PathBuf::from(event["display"]["path"].as_str()?);
        let args = &event["args"];
        let append = match args["mode"].as_str().unwrap_or("overwrite") {
            "append" => true,
            "overwrite" => false,
            _ => return None,
        };
        path.is_absolute().then_some(Self {
            path,
            content: args["content"].as_str()?.to_owned(),
            append,
        })
    }
}

struct Writes<'a> {
    post_call: Option<&'a str>,
    pending: HashMap<String, Write>,
    completed: Vec<(String, Write)>,
}

impl Writes<'_> {
    fn observe(&mut self, event: &Value) {
        let Some(id) = optional_str(event, "toolCallId")
            .filter(|id| !id.is_empty() && self.post_call.is_none_or(|post| post == *id))
        else {
            return;
        };
        match optional_str(event, "type") {
            Some("tool.call") if event["name"] == "Write" => {
                if let Some(write) = Write::from_call(event) {
                    self.pending.insert(id.to_owned(), write);
                }
            }
            Some("tool.result") => {
                let result = &event["result"];
                if let Some(write) = self.pending.remove(id)
                    && result.is_object()
                    && !failed(result)
                {
                    self.completed.push((id.to_owned(), write));
                }
            }
            _ => {}
        }
    }
}
