use super::{add_text, add_tool, failed, optional_str, parse};
use crate::authorship::checkpoint_transcripts::MAX_TRANSCRIPT_BYTES;
use crate::authorship::transcript::AiTranscript;
use crate::mdm::utils::home_dir;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("HERMES_HOME").filter(|home| !home.is_empty()) {
        return PathBuf::from(home);
    }
    let suffix = std::env::var("HERMES_DATA_DIR_SUFFIX").unwrap_or_default();
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData/Local"))
            .join(format!("hermes{suffix}"))
    }
    #[cfg(not(windows))]
    {
        home_dir().join(format!(".hermes{suffix}"))
    }
}

pub(super) fn file_paths(data: &Value, cwd: &str, is_post: bool) -> Vec<PathBuf> {
    if std::env::var("TERMINAL_ENV").is_ok_and(|env| !env.is_empty() && env != "local") {
        return vec![];
    }
    if is_post {
        let result = &data["extra"]["result"];
        let decoded;
        let result = if let Some(raw) = result.as_str() {
            decoded = serde_json::from_str(raw).unwrap_or(Value::Null);
            &decoded
        } else {
            result
        };
        if failed(result) {
            return vec![];
        }
        if result["files_modified"].is_array() {
            return absolute_paths(&result["files_modified"]);
        }
        return optional_str(result, "resolved_path")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .into_iter()
            .collect();
    }
    if let Some(paths) = data["extra"].get("resolved_paths") {
        return absolute_paths(paths);
    }

    let input = &data["tool_input"];
    let mut paths = Vec::new();
    if let Some(path) = optional_str(input, "path").filter(|path| !path.is_empty()) {
        paths.push(path.to_owned());
    }
    if optional_str(data, "tool_name") == Some("patch")
        && optional_str(input, "mode") == Some("patch")
        && let Some(patch) = optional_str(input, "patch")
    {
        parse::collect_apply_patch_paths_from_text(patch, &mut paths);
    }
    paths
        .into_iter()
        .map(|path| {
            if let Some(relative) = path.strip_prefix("~/") {
                home_dir().join(relative)
            } else {
                parse::resolve_absolute(&path, cwd)
            }
        })
        .collect()
}

fn absolute_paths(paths: &Value) -> Vec<PathBuf> {
    paths
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect()
}

pub(super) fn read_session(
    session: &str,
    model: &mut Option<String>,
    transcript: &mut AiTranscript,
) {
    let db = home().join("state.db");
    let Ok(connection) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return;
    };
    let _ = connection.busy_timeout(Duration::from_millis(50));
    if let Ok(value) = connection.query_row(
        "SELECT model FROM sessions WHERE id = ?1",
        [session],
        |row| row.get::<_, String>(0),
    ) {
        *model = Some(value);
    }

    let Ok(mut statement) = connection.prepare(
        "SELECT role, substr(content, 1, ?2), substr(tool_calls, 1, ?2)
         FROM (SELECT id, role, content, tool_calls FROM messages
               WHERE session_id = ?1 AND role IN ('user', 'assistant')
               ORDER BY id DESC LIMIT 512)
         ORDER BY id",
    ) else {
        return;
    };
    let Ok(rows) = statement.query_map(
        rusqlite::params![session, MAX_TRANSCRIPT_BYTES / 4],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    ) else {
        return;
    };
    for (role, content, calls) in rows.flatten() {
        if let Some(content) = content {
            let content = if content.starts_with('[') {
                serde_json::from_str::<Vec<Value>>(&content)
                    .map(Value::Array)
                    .unwrap_or(Value::String(content))
            } else {
                Value::String(content)
            };
            add_text(transcript, &role, &content, None);
        }
        if role == "assistant"
            && let Some(calls) = calls
            && let Ok(calls) = serde_json::from_str::<Vec<Value>>(&calls)
        {
            for call in calls {
                add_tool(transcript, &call["function"], "arguments", None);
            }
        }
    }
}
