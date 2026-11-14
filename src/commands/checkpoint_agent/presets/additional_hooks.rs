pub(crate) mod hermes;
mod kimi;
mod trae;

use super::parse::{self, optional_str};
use super::{AgentPreset, ParsedHookEvent, PostFileEdit, PreFileEdit, PresetContext};
use crate::authorship::checkpoint_transcripts::MAX_TRANSCRIPT_BYTES;
use crate::authorship::transcript::{AiTranscript, Message};
use crate::authorship::working_log::AgentId;
use crate::error::GitAiError;
use crate::mdm::utils::home_dir;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileHookAgent {
    Qoder,
    Hermes,
    Trae,
    KimiCode,
}

impl FileHookAgent {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Qoder => "qoder",
            Self::Hermes => "hermes",
            Self::Trae => "trae",
            Self::KimiCode => "kimi-code",
        }
    }

    pub(crate) fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Qoder => &[
                "Write",
                "Edit",
                "MultiEdit",
                "create_file",
                "search_replace",
                "edit_file",
            ],
            Self::Hermes => &["write_file", "patch"],
            Self::Trae | Self::KimiCode => &["Write", "Edit"],
        }
    }

    pub(crate) fn edit_events(self) -> [&'static str; 2] {
        match self {
            Self::Hermes => ["pre_tool_call", "post_tool_call"],
            _ => ["PreToolUse", "PostToolUse"],
        }
    }
}

pub(crate) fn agent_home(env: &str, default_dir: &str) -> PathBuf {
    std::env::var_os(env)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(default_dir))
}

impl AgentPreset for FileHookAgent {
    fn parse(&self, input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: Value = serde_json::from_str(input)?;
        let event = parse::required_str(&data, "hook_event_name")?;
        let Some(session) = optional_str(&data, "session_id").filter(|id| !id.is_empty()) else {
            return Ok(vec![]);
        };
        let cwd = if let Some(cwd) = optional_str(&data, "cwd") {
            PathBuf::from(cwd)
        } else if *self == Self::Trae && matches!(event, "UserPromptSubmit" | "Stop") {
            std::env::current_dir()?
        } else {
            return Ok(vec![]);
        };
        let Some(cwd) = cwd.to_str().filter(|cwd| Path::new(cwd).is_absolute()) else {
            return Ok(vec![]);
        };
        let mut model = optional_str(&data, "model").map(str::to_owned);
        if *self == Self::KimiCode && event == "Stop" {
            return Ok(kimi::write_checkpoints(
                self.context(session, trace_id, cwd, model),
                kimi::WriteHook::Stop,
            )
            .unwrap_or_default());
        }
        if *self == Self::Trae && matches!(event, "UserPromptSubmit" | "Stop") {
            trae::record_message(&data, event, session, Path::new(cwd));
            return Ok(vec![]);
        }

        let [before, after] = self.edit_events();
        let Some(tool) =
            optional_str(&data, "tool_name").filter(|tool| self.tool_names().contains(tool))
        else {
            return Ok(vec![]);
        };
        if event != before && event != after {
            return Ok(vec![]);
        }
        let is_post = event == after;
        let extra = &data["extra"];
        if is_post && (failed(&data["tool_response"]) || failed(extra)) {
            return Ok(vec![]);
        }
        let tool_use_id = parse::optional_str_multi(&data, &["tool_use_id", "tool_call_id"])
            .or_else(|| optional_str(extra, "tool_call_id"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if matches!(self, Self::Hermes | Self::KimiCode) && tool_use_id.is_none() {
            return Ok(vec![]);
        }
        if *self == Self::KimiCode
            && is_post
            && tool == "Write"
            && let Some(id) = tool_use_id.as_deref()
        {
            return Ok(kimi::write_checkpoints(
                self.context(session, trace_id, cwd, model),
                kimi::WriteHook::Post(id),
            )
            .unwrap_or_default());
        }

        let mut paths = if *self == Self::Hermes {
            hermes::file_paths(&data, cwd, is_post)
        } else {
            let mut paths = parse::file_paths_from_tool_input(&data, cwd);
            if paths.is_empty()
                && let Some(path) =
                    parse::optional_str_multi(&data["tool_input"], &["filePath", "target_file"])
                && !path.is_empty()
            {
                paths.push(parse::resolve_absolute(path, cwd));
            }
            paths
        };
        if paths.is_empty()
            && *self == Self::Qoder
            && let Ok(path) = std::env::var("QODER_TOOL_INPUT_FILE_PATH")
            && !path.is_empty()
        {
            paths.push(parse::resolve_absolute(&path, cwd));
        }
        if *self == Self::Trae && !is_post {
            paths.retain(|path| trae::repository_for_file(path).is_some());
        }
        if paths.is_empty() {
            return Ok(vec![]);
        }
        if !is_post {
            let context = self.context(session, trace_id, cwd, model);
            let mut events = if *self == Self::KimiCode {
                let Some(events) =
                    kimi::write_checkpoints(context.clone(), kimi::WriteHook::Pre(&paths))
                else {
                    return Ok(vec![]);
                };
                events
            } else {
                Vec::new()
            };
            events.push(ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: paths,
                dirty_files: None,
                tool_use_id,
            }));
            return Ok(events);
        }
        let mut transcript = AiTranscript::new();
        if *self == Self::KimiCode && !kimi::read_session(session, &mut model, &mut transcript) {
            return Ok(vec![]);
        }
        match self {
            Self::Qoder => read_qoder(&data, cwd, &mut model, &mut transcript),
            Self::Hermes => hermes::read_session(session, &mut model, &mut transcript),
            Self::Trae | Self::KimiCode => {}
        }

        let context = self.context(session, trace_id, cwd, model);
        let edit = PostFileEdit {
            context,
            file_paths: paths,
            dirty_files: None,
            stream_source: None,
            tool_use_id,
        };
        if *self == Self::Trae {
            return Ok(trae::tool_checkpoints(edit, &data, tool));
        }
        Ok(vec![post_file_edit(edit, transcript)])
    }
}

impl FileHookAgent {
    fn context(
        self,
        session: &str,
        trace_id: &str,
        cwd: &str,
        model: Option<String>,
    ) -> PresetContext {
        PresetContext {
            agent_id: AgentId {
                tool: self.id().to_owned(),
                id: session.to_owned(),
                model: model.unwrap_or_else(|| "unknown".to_owned()),
            },
            external_session_id: session.to_owned(),
            trace_id: trace_id.to_owned(),
            cwd: PathBuf::from(cwd),
            metadata: Default::default(),
        }
    }
}

fn post_file_edit(edit: PostFileEdit, mut transcript: AiTranscript) -> ParsedHookEvent {
    limit_transcript(&mut transcript);
    if transcript.messages.is_empty() {
        ParsedHookEvent::PostFileEdit(edit)
    } else {
        ParsedHookEvent::PostFileEditWithTranscript(edit, transcript)
    }
}

fn failed(response: &Value) -> bool {
    response["success"].as_bool() == Some(false)
        || response["is_error"].as_bool() == Some(true)
        || response["isError"].as_bool() == Some(true)
        || matches!(
            optional_str(response, "status"),
            Some("error" | "failed" | "blocked" | "cancelled" | "timeout")
        )
        || response.get("error").is_some_and(|error| {
            !matches!(error, Value::Null | Value::Bool(false)) && error.as_str() != Some("")
        })
}

fn read_qoder(data: &Value, cwd: &str, model: &mut Option<String>, transcript: &mut AiTranscript) {
    let Some(path) = optional_str(data, "transcript_path").filter(|path| !path.is_empty()) else {
        return;
    };
    for row in jsonl(&parse::resolve_absolute(path, cwd)) {
        let message = &row["message"];
        let role = optional_str(&row, "type").or_else(|| optional_str(message, "role"));
        let timestamp = optional_str(&row, "timestamp");
        match role {
            Some("user") => add_text(transcript, "user", &message["content"], timestamp),
            Some("assistant") => {
                if let Some(value) = optional_str(message, "model") {
                    *model = Some(value.to_owned());
                }
                let content = &message["content"];
                if let Some(parts) = content.as_array() {
                    for part in parts {
                        if part["type"] == "tool_use" {
                            add_tool(transcript, part, "input", timestamp);
                        } else {
                            add_text(transcript, "assistant", part, timestamp);
                        }
                    }
                } else {
                    add_text(transcript, "assistant", content, timestamp);
                }
            }
            _ => {}
        }
    }
}

fn add_text(transcript: &mut AiTranscript, role: &str, value: &Value, timestamp: Option<&str>) {
    if let Some(parts) = value.as_array() {
        for part in parts {
            add_text(transcript, role, part, timestamp);
        }
        return;
    }
    let text = value.as_str().or_else(|| match value["type"].as_str() {
        Some("text" | "input_text" | "output_text") => optional_str(value, "text"),
        _ => None,
    });
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return;
    };
    transcript.add_message(match role {
        "user" => Message::user(text.to_owned(), timestamp.map(str::to_owned)),
        _ => Message::assistant(text.to_owned(), timestamp.map(str::to_owned)),
    });
}

fn add_tool(
    transcript: &mut AiTranscript,
    call: &Value,
    input_field: &str,
    timestamp: Option<&str>,
) {
    let Some(name) = optional_str(call, "name") else {
        return;
    };
    let input = &call[input_field];
    let input = if let Some(arguments) = input.as_str() {
        serde_json::from_str(arguments).unwrap_or_else(|_| input.clone())
    } else {
        input.clone()
    };
    transcript.add_message(Message::ToolUse {
        name: name.to_owned(),
        input,
        timestamp: timestamp.map(str::to_owned),
    });
}

fn jsonl(path: &Path) -> impl Iterator<Item = Value> {
    path.is_file()
        .then(|| File::open(path).and_then(read_jsonl).ok())
        .flatten()
        .into_iter()
        .flatten()
}

fn read_jsonl(mut file: impl Read + Seek) -> std::io::Result<impl Iterator<Item = Value>> {
    // Bound hook I/O while leaving room for agent events excluded from the transcript.
    let window = (MAX_TRANSCRIPT_BYTES * 8) as u64;
    let start = file.seek(SeekFrom::End(0))?.saturating_sub(window);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(window));
    if start > 0 {
        reader.skip_until(b'\n')?;
    }
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok()))
}

fn limit_transcript(transcript: &mut AiTranscript) {
    let mut bytes = "{\"messages\":[]}".len();
    let first = transcript
        .messages
        .iter()
        .rposition(|message| {
            bytes +=
                serde_json::to_vec(message).map_or(MAX_TRANSCRIPT_BYTES, |json| json.len() + 1);
            bytes > MAX_TRANSCRIPT_BYTES
        })
        .map_or(0, |index| index + 1);
    transcript.messages.drain(..first);
}
