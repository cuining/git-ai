use super::TokenCounts;
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeSessionSnapshot {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub created_at_ms: i64,
    pub observations: Vec<NativeUsageObservation>,
    pub replay_diagnostics: Vec<NativeReplayDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeUsageObservation {
    pub entry_key: String,
    pub message_id: String,
    pub model: String,
    pub provider_id: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub timestamp_secs: u32,
    pub tokens: TokenCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeReplayDiagnostic {
    pub entry_key: String,
    pub kind: NativeReplayKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NativeReplayKind {
    BeforeSessionCreation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeSnapshotError {
    Unsupported(NativeUnsupported),
    Invalid(NativeInvalid),
    ReadFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeUnsupported {
    Schema,
    NotSqlite,
    SessionMessage,
    CumulativeOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeInvalid {
    pub entry_key: Option<String>,
    pub field: NativeField,
    pub reason: NativeInvalidReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeField {
    SessionId,
    SessionParentId,
    SessionCreatedAt,
    MessageId,
    MessageCreatedAt,
    MessageUpdatedAt,
    Role,
    Model,
    Provider,
    Input,
    Output,
    Reasoning,
    CacheRead,
    CacheWrite,
    Total,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeInvalidReason {
    Missing,
    Malformed,
    OutOfRange,
    Overflow,
    AmbiguousReplayBoundary,
}

#[derive(Debug)]
pub(crate) struct NativeSessionRead {
    pub observed_parent_session_id: Option<String>,
    pub usage: Result<NativeSessionSnapshot, NativeSnapshotError>,
}

pub(crate) fn read_native_session(path: &Path, session_id: &str) -> NativeSessionRead {
    let mut observed_parent_session_id = None;
    let usage = read_session_snapshot(path, session_id, &mut observed_parent_session_id);
    NativeSessionRead {
        observed_parent_session_id,
        usage,
    }
}

fn read_session_snapshot(
    path: &Path,
    session_id: &str,
    observed_parent_session_id: &mut Option<String>,
) -> Result<NativeSessionSnapshot, NativeSnapshotError> {
    validate_text(session_id, NativeField::SessionId, None)?;
    let mut connection =
        crate::sqlite::open_with_flags_and_memory_limits(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    let session_columns = table_columns(&transaction, "session")?;
    if !["id", "parent_id", "time_created"]
        .iter()
        .all(|column| session_columns.contains(*column))
    {
        return Err(NativeSnapshotError::Unsupported(NativeUnsupported::Schema));
    }

    let mut statement =
        transaction.prepare("SELECT id, parent_id, time_created FROM session WHERE id=?1")?;
    let mut rows = statement.query([session_id])?;
    let row = rows
        .next()?
        .ok_or_else(|| invalid(NativeField::SessionId, NativeInvalidReason::Missing, None))?;
    let native_id = required_text(row.get_ref(0)?, NativeField::SessionId, None)?;
    if native_id != session_id {
        return Err(invalid(
            NativeField::SessionId,
            NativeInvalidReason::Malformed,
            None,
        ));
    }
    let parent_session_id = match row.get_ref(1)? {
        ValueRef::Null => None,
        value => Some(required_text(value, NativeField::SessionParentId, None)?),
    };
    let created_at_ms = required_integer(row.get_ref(2)?, NativeField::SessionCreatedAt, None)?;
    if rows.next()?.is_some() {
        return Err(invalid(
            NativeField::SessionId,
            NativeInvalidReason::Malformed,
            None,
        ));
    }
    drop(rows);
    drop(statement);

    *observed_parent_session_id = parent_session_id.clone();

    let newer_columns = table_columns(&transaction, "session_message")?;
    if !newer_columns.is_empty() {
        if !newer_columns.contains("session_id") {
            return Err(NativeSnapshotError::Unsupported(NativeUnsupported::Schema));
        }
        let has_newer_rows: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_message WHERE session_id=?1)",
            [session_id],
            |row| row.get(0),
        )?;
        if has_newer_rows {
            return Err(NativeSnapshotError::Unsupported(
                NativeUnsupported::SessionMessage,
            ));
        }
    }

    let message_columns = table_columns(&transaction, "message")?;
    if !["id", "session_id", "time_created", "time_updated", "data"]
        .iter()
        .all(|column| message_columns.contains(*column))
    {
        return Err(NativeSnapshotError::Unsupported(NativeUnsupported::Schema));
    }

    let mut snapshot = NativeSessionSnapshot {
        session_id: native_id,
        parent_session_id,
        created_at_ms,
        observations: Vec::new(),
        replay_diagnostics: Vec::new(),
    };
    let mut statement = transaction.prepare(MESSAGE_QUERY)?;
    let mut rows = statement.query([session_id])?;
    let mut message_keys = HashSet::new();
    while let Some(row) = rows.next()? {
        let message_id = required_text(row.get_ref(0)?, NativeField::MessageId, None)?;
        let entry_key = format!(
            "opencode:{}:{}:{}",
            session_id.len(),
            session_id,
            message_id
        );
        let key = Some(entry_key.as_str());
        if !message_keys.insert(entry_key.clone()) {
            return Err(invalid(
                NativeField::MessageId,
                NativeInvalidReason::Malformed,
                key,
            ));
        }
        if row.get::<_, i64>(3)? != 1 {
            return Err(invalid(
                NativeField::Role,
                NativeInvalidReason::Malformed,
                key,
            ));
        }
        let role = projected_text(row, 4, NativeField::Role, key)?;
        match role.as_str() {
            "user" => continue,
            "assistant" => {}
            _ => {
                return Err(invalid(
                    NativeField::Role,
                    NativeInvalidReason::Malformed,
                    key,
                ));
            }
        }

        required_integer(row.get_ref(1)?, NativeField::MessageCreatedAt, key)?;
        let updated_at_ms = required_integer(row.get_ref(2)?, NativeField::MessageUpdatedAt, key)?;
        let message_created_at_ms = projected_integer(row, 6, NativeField::MessageCreatedAt, key)?;
        let timestamp_secs = u32::try_from(message_created_at_ms / 1000).map_err(|_| {
            invalid(
                NativeField::MessageCreatedAt,
                NativeInvalidReason::OutOfRange,
                key,
            )
        })?;
        if message_created_at_ms < created_at_ms {
            snapshot.replay_diagnostics.push(NativeReplayDiagnostic {
                entry_key,
                kind: NativeReplayKind::BeforeSessionCreation,
            });
            continue;
        }
        if message_created_at_ms == created_at_ms {
            return Err(invalid(
                NativeField::MessageCreatedAt,
                NativeInvalidReason::AmbiguousReplayBoundary,
                key,
            ));
        }

        let model = projected_text(row, 8, NativeField::Model, key)?;
        let provider_id = projected_text(row, 10, NativeField::Provider, key)?;
        let input = projected_integer(row, 12, NativeField::Input, key)? as u64;
        let visible_output = projected_integer(row, 14, NativeField::Output, key)? as u64;
        let reasoning = projected_integer(row, 16, NativeField::Reasoning, key)? as u64;
        let cache_read = projected_integer(row, 18, NativeField::CacheRead, key)? as u64;
        let cache_write = projected_integer(row, 20, NativeField::CacheWrite, key)? as u64;
        let overflow = || invalid(NativeField::Total, NativeInvalidReason::Overflow, key);
        let output = visible_output.checked_add(reasoning).ok_or_else(overflow)?;
        let total = input
            .checked_add(output)
            .and_then(|total| total.checked_add(cache_read))
            .and_then(|total| total.checked_add(cache_write))
            .ok_or_else(overflow)?;
        snapshot.observations.push(NativeUsageObservation {
            entry_key,
            message_id,
            model,
            provider_id,
            created_at_ms: message_created_at_ms,
            updated_at_ms,
            timestamp_secs,
            tokens: TokenCounts {
                input,
                output,
                cache_read,
                cache_write,
                reasoning_output: Some(reasoning),
                total,
            },
        });
    }
    drop(rows);
    drop(statement);

    if snapshot.observations.is_empty() && snapshot.replay_diagnostics.is_empty() {
        let cumulative_columns = [
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
        ];
        let present_columns: Vec<_> = cumulative_columns
            .iter()
            .filter(|column| session_columns.contains(**column))
            .collect();
        if !present_columns.is_empty() {
            if present_columns.len() != cumulative_columns.len() {
                return Err(NativeSnapshotError::Unsupported(NativeUnsupported::Schema));
            }
            let sql = format!(
                "SELECT {} FROM session WHERE id=?1",
                cumulative_columns.join(",")
            );
            let only_known_zero = transaction.query_row(&sql, [session_id], |row| {
                for index in 0..cumulative_columns.len() {
                    if row.get_ref(index)? != ValueRef::Integer(0) {
                        return Ok(false);
                    }
                }
                Ok(true)
            })?;
            if !only_known_zero {
                return Err(NativeSnapshotError::Unsupported(
                    NativeUnsupported::CumulativeOnly,
                ));
            }
        }
    }

    transaction.commit()?;
    Ok(snapshot)
}

const MESSAGE_QUERY: &str = "
WITH native AS (
    SELECT id, time_created, time_updated, json_valid(data) AS valid_json,
           CASE WHEN json_valid(data) THEN data ELSE '{}' END AS metadata
    FROM message WHERE session_id=?1
)
SELECT id, time_created, time_updated, valid_json,
       json_type(metadata, '$.role'),
       CASE WHEN json_type(metadata, '$.role')='text' THEN json_extract(metadata, '$.role') END,
       json_type(metadata, '$.time.created'),
       CASE WHEN json_type(metadata, '$.time.created')='integer' THEN json_extract(metadata, '$.time.created') END,
       json_type(metadata, '$.modelID'),
       CASE WHEN json_type(metadata, '$.modelID')='text' THEN json_extract(metadata, '$.modelID') END,
       json_type(metadata, '$.providerID'),
       CASE WHEN json_type(metadata, '$.providerID')='text' THEN json_extract(metadata, '$.providerID') END,
       json_type(metadata, '$.tokens.input'),
       CASE WHEN json_type(metadata, '$.tokens.input')='integer' THEN json_extract(metadata, '$.tokens.input') END,
       json_type(metadata, '$.tokens.output'),
       CASE WHEN json_type(metadata, '$.tokens.output')='integer' THEN json_extract(metadata, '$.tokens.output') END,
       json_type(metadata, '$.tokens.reasoning'),
       CASE WHEN json_type(metadata, '$.tokens.reasoning')='integer' THEN json_extract(metadata, '$.tokens.reasoning') END,
       json_type(metadata, '$.tokens.cache.read'),
       CASE WHEN json_type(metadata, '$.tokens.cache.read')='integer' THEN json_extract(metadata, '$.tokens.cache.read') END,
       json_type(metadata, '$.tokens.cache.write'),
       CASE WHEN json_type(metadata, '$.tokens.cache.write')='integer' THEN json_extract(metadata, '$.tokens.cache.write') END
FROM native ORDER BY id
";

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<HashSet<String>, NativeSnapshotError> {
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
    Ok(statement
        .query_map([table], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn projected_text(
    row: &Row<'_>,
    index: usize,
    field: NativeField,
    key: Option<&str>,
) -> Result<String, NativeSnapshotError> {
    validate_json_type(row, index, "text", field, key)?;
    required_text(row.get_ref(index + 1)?, field, key)
}

fn projected_integer(
    row: &Row<'_>,
    index: usize,
    field: NativeField,
    key: Option<&str>,
) -> Result<i64, NativeSnapshotError> {
    validate_json_type(row, index, "integer", field, key)?;
    required_integer(row.get_ref(index + 1)?, field, key)
}

fn validate_json_type(
    row: &Row<'_>,
    index: usize,
    expected: &str,
    field: NativeField,
    key: Option<&str>,
) -> Result<(), NativeSnapshotError> {
    let kind: Option<String> = row.get(index)?;
    match kind.as_deref() {
        Some(kind) if kind == expected => Ok(()),
        None | Some("null") => Err(invalid(field, missing_reason(field), key)),
        _ => Err(invalid(field, NativeInvalidReason::Malformed, key)),
    }
}

fn required_integer(
    value: ValueRef<'_>,
    field: NativeField,
    key: Option<&str>,
) -> Result<i64, NativeSnapshotError> {
    match value {
        ValueRef::Integer(value) if value >= 0 => Ok(value),
        ValueRef::Null => Err(invalid(field, missing_reason(field), key)),
        ValueRef::Integer(_) | ValueRef::Real(_) => {
            Err(invalid(field, NativeInvalidReason::OutOfRange, key))
        }
        _ => Err(invalid(field, NativeInvalidReason::Malformed, key)),
    }
}

fn required_text(
    value: ValueRef<'_>,
    field: NativeField,
    key: Option<&str>,
) -> Result<String, NativeSnapshotError> {
    match value {
        ValueRef::Text(bytes) => {
            let value = std::str::from_utf8(bytes)
                .map_err(|_| invalid(field, NativeInvalidReason::Malformed, key))?;
            validate_text(value, field, key)?;
            Ok(value.to_owned())
        }
        ValueRef::Null => Err(invalid(field, NativeInvalidReason::Missing, key)),
        _ => Err(invalid(field, NativeInvalidReason::Malformed, key)),
    }
}

fn validate_text(
    value: &str,
    field: NativeField,
    key: Option<&str>,
) -> Result<(), NativeSnapshotError> {
    if value.trim().is_empty() {
        return Err(invalid(field, NativeInvalidReason::Malformed, key));
    }
    Ok(())
}

fn missing_reason(field: NativeField) -> NativeInvalidReason {
    match field {
        NativeField::SessionCreatedAt | NativeField::MessageCreatedAt => {
            NativeInvalidReason::AmbiguousReplayBoundary
        }
        _ => NativeInvalidReason::Missing,
    }
}

fn invalid(
    field: NativeField,
    reason: NativeInvalidReason,
    key: Option<&str>,
) -> NativeSnapshotError {
    NativeSnapshotError::Invalid(NativeInvalid {
        entry_key: key.map(str::to_owned),
        field,
        reason,
    })
}

impl From<rusqlite::Error> for NativeSnapshotError {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(error, _) = error
            && error.code == rusqlite::ffi::ErrorCode::NotADatabase
        {
            return Self::Unsupported(NativeUnsupported::NotSqlite);
        }
        Self::ReadFailure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    const CREATED: i64 = 1_767_225_600_000;
    const SCHEMA: &str = include_str!("../../tests/fixtures/opencode-enterprise/schema.sql");

    struct Database {
        _dir: tempfile::TempDir,
        path: PathBuf,
        writer: Connection,
    }

    impl Database {
        fn new() -> Self {
            Self::with_schema(SCHEMA)
        }

        fn with_schema(schema: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("opencode.db");
            let writer = Connection::open(&path).unwrap();
            writer.execute_batch(schema).unwrap();
            let db = Self {
                _dir: dir,
                path,
                writer,
            };
            db.session("session", Some("native-parent"), CREATED);
            db
        }

        fn session(&self, id: &str, parent: Option<&str>, created: i64) {
            self.writer
                .execute(
                    "INSERT INTO session(id, parent_id, time_created, time_updated) VALUES(?, ?, ?, ?)",
                    params![id, parent, created, created],
                )
                .unwrap();
        }

        fn message(&self, id: &str, session: &str, data: &Value) {
            let created = data["time"]["created"].as_i64().unwrap_or(CREATED + 1000);
            self.writer
                .execute(
                    "INSERT INTO message(id, session_id, time_created, time_updated, data) VALUES(?, ?, ?, ?, ?)",
                    params![id, session, created, created + 1000, data.to_string()],
                )
                .unwrap();
        }

        fn replace_data(&self, id: &str, data: &Value) {
            self.writer
                .execute(
                    "UPDATE message SET data=? WHERE id=?",
                    params![data.to_string(), id],
                )
                .unwrap();
        }

        fn read(&self) -> Result<NativeSessionSnapshot, NativeSnapshotError> {
            read_native_session(&self.path, "session").usage
        }
    }

    fn assistant(created: i64) -> Value {
        json!({
            "role": "assistant",
            "time": {"created": created, "completed": created + 1000},
            "parentID": "user-message-only",
            "modelID": "gpt-5",
            "providerID": "openai",
            "tokens": {
                "input": 100,
                "output": 40,
                "reasoning": 10,
                "cache": {"read": 30, "write": 20}
            }
        })
    }

    fn assert_invalid(
        result: Result<NativeSessionSnapshot, NativeSnapshotError>,
        field: NativeField,
    ) -> NativeInvalid {
        let Err(NativeSnapshotError::Invalid(invalid)) = result else {
            panic!("expected unavailable {field:?}, got {result:?}");
        };
        assert_eq!(invalid.field, field);
        invalid
    }

    #[test]
    fn reads_checked_native_fixture_and_session_ancestry() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/opencode-sqlite/opencode.db");
        let snapshot = read_native_session(&path, "test-session-123")
            .usage
            .unwrap();
        assert_eq!(snapshot.session_id, "test-session-123");
        assert_eq!(
            snapshot.parent_session_id.as_deref(),
            Some("parent-session-456")
        );
        assert_eq!(snapshot.created_at_ms, 1_706_459_830_000);
        assert_eq!(snapshot.observations.len(), 1);
        let usage = &snapshot.observations[0];
        assert_eq!(usage.message_id, "msg-assistant-sql-001");
        assert_eq!(usage.timestamp_secs, 1_706_459_831);
        assert_eq!(usage.model, "gpt-5");
        assert_eq!(usage.provider_id, "openai");
        assert_eq!(usage.tokens.input, 10);
        assert_eq!(usage.tokens.output, 20);
        assert_eq!(usage.tokens.total, 30);
        assert!(snapshot.replay_diagnostics.is_empty());
    }

    #[test]
    fn normalizes_caches_and_reasoning_once_without_reading_duplicate_representations() {
        let db = Database::new();
        db.message("assistant", "session", &assistant(CREATED + 1000));
        db.message(
            "user",
            "session",
            &json!({"role": "user", "text": "private prompt"}),
        );
        db.writer
            .execute_batch(
                "ALTER TABLE session ADD COLUMN tokens_input INTEGER DEFAULT 100;
                 ALTER TABLE session ADD COLUMN tokens_output INTEGER DEFAULT 40;
                 ALTER TABLE session ADD COLUMN tokens_reasoning INTEGER DEFAULT 10;
                 ALTER TABLE session ADD COLUMN tokens_cache_read INTEGER DEFAULT 30;
                 ALTER TABLE session ADD COLUMN tokens_cache_write INTEGER DEFAULT 20;
                 INSERT INTO part VALUES('step', 'assistant', 'session', 1, 1,
                   '{\"type\":\"step-finish\",\"tokens\":{\"input\":100,\"output\":40,\"reasoning\":10,\"cache\":{\"read\":30,\"write\":20}}}');
                 INSERT INTO part VALUES('tool', 'assistant', 'session', 1, 1,
                   'private malformed tool payload');",
            )
            .unwrap();
        let snapshot = db.read().unwrap();
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(
            snapshot.observations[0].tokens,
            TokenCounts {
                input: 100,
                output: 50,
                cache_read: 30,
                cache_write: 20,
                reasoning_output: Some(10),
                total: 200,
            }
        );
        assert!(!format!("{snapshot:?}").contains("private"));
    }

    #[test]
    fn separates_sessions_sharing_a_database_and_namespaces_source_identity() {
        let db = Database::new();
        db.session("other", None, CREATED);
        db.message("first", "session", &assistant(CREATED + 1000));
        db.message("second", "other", &assistant(CREATED + 2000));
        let first = db.read().unwrap();
        let second = read_native_session(&db.path, "other").usage.unwrap();
        assert_eq!(first.observations.len(), 1);
        assert_eq!(second.observations.len(), 1);
        assert_eq!(first.observations[0].message_id, "first");
        assert_eq!(second.observations[0].message_id, "second");
        assert_ne!(
            first.observations[0].entry_key,
            second.observations[0].entry_key
        );
        assert!(first.observations[0].entry_key.starts_with("opencode:"));
        assert_eq!(second.parent_session_id, None);
        db.writer
            .execute("UPDATE message SET session_id='other' WHERE id='first'", [])
            .unwrap();
        let moved = read_native_session(&db.path, "other").usage.unwrap();
        let moved_first = moved
            .observations
            .iter()
            .find(|row| row.message_id == "first")
            .unwrap();
        assert_ne!(first.observations[0].entry_key, moved_first.entry_key);
    }

    #[test]
    fn full_rescans_see_wal_only_tied_updates_and_decreases() {
        let db = Database::new();
        db.writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        db.message("first", "session", &assistant(CREATED + 1000));
        db.writer
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let before = db.read().unwrap();
        let main_before = std::fs::read(&db.path).unwrap();
        let mut lower = assistant(CREATED + 1000);
        lower["tokens"]["input"] = json!(1);
        db.replace_data("first", &lower);
        db.message("second", "session", &assistant(CREATED + 1000));
        db.message("third", "session", &assistant(CREATED + 1000));
        assert_eq!(std::fs::read(&db.path).unwrap(), main_before);
        assert!(
            std::fs::metadata(db.path.with_extension("db-wal"))
                .unwrap()
                .len()
                > 0
        );
        let after = db.read().unwrap();
        assert_eq!(before.observations[0].tokens.input, 100);
        assert_eq!(after.observations.len(), 3);
        assert_eq!(after.observations[0].tokens.input, 1);
        assert_eq!(after.observations[0].tokens.total, 101);
        assert!(
            after
                .observations
                .iter()
                .all(|row| row.updated_at_ms == CREATED + 2000)
        );
    }

    #[test]
    fn full_rescans_keep_model_and_timestamp_corrections_and_authoritative_deletions() {
        let db = Database::new();
        db.message("first", "session", &assistant(CREATED + 1000));
        let initial = db.read().unwrap();
        let mut moved = assistant(CREATED + 601_000);
        moved["modelID"] = json!("changed-model");
        moved["providerID"] = json!("changed-provider");
        db.replace_data("first", &moved);
        let changed = db.read().unwrap();
        assert_eq!(
            changed.observations[0].entry_key,
            initial.observations[0].entry_key
        );
        assert_eq!(changed.observations[0].model, "changed-model");
        assert_eq!(changed.observations[0].provider_id, "changed-provider");
        assert_eq!(
            changed.observations[0].timestamp_secs,
            (CREATED / 1000) as u32 + 601
        );
        db.writer
            .execute("DELETE FROM message WHERE id='first'", [])
            .unwrap();
        assert!(db.read().unwrap().observations.is_empty());
    }

    #[test]
    fn rereading_and_reopening_returns_the_same_snapshot() {
        let db = Database::new();
        db.message("assistant", "session", &assistant(CREATED + 1000));
        let first = db.read().unwrap();
        assert_eq!(db.read().unwrap(), first);
        drop(db.writer);
        assert_eq!(
            read_native_session(&db.path, "session").usage.unwrap(),
            first
        );
    }

    #[test]
    fn excludes_cloned_fork_history_with_new_ids_before_session_creation() {
        let db = Database::new();
        db.message("fork-copy-new-id", "session", &assistant(CREATED - 1000));
        db.message("real-new-id", "session", &assistant(CREATED + 1000));
        let snapshot = db.read().unwrap();
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(snapshot.observations[0].message_id, "real-new-id");
        assert_eq!(snapshot.replay_diagnostics.len(), 1);
        assert_eq!(
            snapshot.replay_diagnostics[0].kind,
            NativeReplayKind::BeforeSessionCreation
        );
        assert!(
            snapshot.replay_diagnostics[0]
                .entry_key
                .contains("fork-copy-new-id")
        );
    }

    #[test]
    fn genuine_child_usage_keeps_session_parent_and_never_uses_message_parent() {
        let db = Database::new();
        db.message("child-message", "session", &assistant(CREATED + 1000));
        let snapshot = db.read().unwrap();
        assert_eq!(snapshot.parent_session_id.as_deref(), Some("native-parent"));
        assert_eq!(snapshot.observations[0].tokens.total, 200);
        db.writer
            .execute("UPDATE session SET parent_id=NULL", [])
            .unwrap();
        assert_eq!(db.read().unwrap().parent_session_id, None);
    }

    #[test]
    fn equal_creation_boundary_is_unavailable_instead_of_an_empty_snapshot() {
        let db = Database::new();
        db.message("ambiguous", "session", &assistant(CREATED));
        let invalid = assert_invalid(db.read(), NativeField::MessageCreatedAt);
        assert_eq!(invalid.reason, NativeInvalidReason::AmbiguousReplayBoundary);
    }

    #[test]
    fn missing_session_or_message_time_is_an_explicit_ambiguous_boundary() {
        let db = Database::with_schema(
            &SCHEMA.replace("time_created INTEGER NOT NULL", "time_created INTEGER"),
        );
        db.message("assistant", "session", &assistant(CREATED + 1000));
        db.writer
            .execute("UPDATE session SET time_created=NULL", [])
            .unwrap();
        let invalid = assert_invalid(db.read(), NativeField::SessionCreatedAt);
        assert_eq!(invalid.reason, NativeInvalidReason::AmbiguousReplayBoundary);
        db.writer
            .execute("UPDATE session SET time_created=?", [CREATED])
            .unwrap();
        let mut missing = assistant(CREATED + 1000);
        missing["time"].as_object_mut().unwrap().remove("created");
        db.replace_data("assistant", &missing);
        let invalid = assert_invalid(db.read(), NativeField::MessageCreatedAt);
        assert_eq!(invalid.reason, NativeInvalidReason::AmbiguousReplayBoundary);
    }

    #[test]
    fn invalid_counts_reject_the_whole_snapshot_without_invented_zero() {
        let db = Database::new();
        db.message("valid", "session", &assistant(CREATED + 1000));
        db.message("invalid", "session", &assistant(CREATED + 2000));
        for (pointer, field) in [
            ("/tokens/input", NativeField::Input),
            ("/tokens/output", NativeField::Output),
            ("/tokens/reasoning", NativeField::Reasoning),
            ("/tokens/cache/read", NativeField::CacheRead),
            ("/tokens/cache/write", NativeField::CacheWrite),
        ] {
            for value in [
                Value::Null,
                json!(-1),
                json!("private bad count"),
                json!(true),
                json!(1.5),
                json!(1e100),
            ] {
                let mut invalid = assistant(CREATED + 2000);
                *invalid.pointer_mut(pointer).unwrap() = value;
                db.replace_data("invalid", &invalid);
                let diagnostic = assert_invalid(db.read(), field);
                assert!(diagnostic.entry_key.is_some());
                assert!(!format!("{diagnostic:?}").contains("private"));
            }
        }
    }

    #[test]
    fn missing_required_count_is_unavailable_and_known_zero_is_preserved() {
        let db = Database::new();
        let mut missing = assistant(CREATED + 1000);
        missing["tokens"]
            .as_object_mut()
            .unwrap()
            .remove("reasoning");
        db.message("assistant", "session", &missing);
        assert_invalid(db.read(), NativeField::Reasoning);
        missing["tokens"] =
            json!({"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}});
        db.replace_data("assistant", &missing);
        let snapshot = db.read().unwrap();
        assert_eq!(snapshot.observations.len(), 1);
        assert_eq!(snapshot.observations[0].tokens.total, 0);
    }

    #[test]
    fn invalid_model_provider_and_timestamp_never_get_defaults() {
        let db = Database::new();
        db.message("assistant", "session", &assistant(CREATED + 1000));
        for (pointer, field, values) in [
            (
                "/modelID",
                NativeField::Model,
                vec![Value::Null, json!(""), json!("  "), json!(23)],
            ),
            (
                "/providerID",
                NativeField::Provider,
                vec![Value::Null, json!(""), json!(false)],
            ),
            (
                "/time/created",
                NativeField::MessageCreatedAt,
                vec![
                    Value::Null,
                    json!(-1),
                    json!(1.5),
                    json!("now"),
                    json!((u32::MAX as i64 + 1) * 1000),
                ],
            ),
        ] {
            for value in values {
                let mut invalid = assistant(CREATED + 1000);
                *invalid.pointer_mut(pointer).unwrap() = value;
                db.replace_data("assistant", &invalid);
                assert_invalid(db.read(), field);
            }
        }
    }

    #[test]
    fn malformed_json_and_unknown_role_never_install_a_partial_snapshot() {
        let db = Database::new();
        db.message("good", "session", &assistant(CREATED + 1000));
        db.message("bad", "session", &assistant(CREATED + 2000));
        db.writer
            .execute(
                "UPDATE message SET data='private malformed payload' WHERE id='bad'",
                [],
            )
            .unwrap();
        let invalid = assert_invalid(db.read(), NativeField::Role);
        assert_eq!(invalid.reason, NativeInvalidReason::Malformed);
        assert!(!format!("{invalid:?}").contains("private"));
        let mut wrong_role = assistant(CREATED + 2000);
        wrong_role["role"] = json!("unrecognized");
        db.replace_data("bad", &wrong_role);
        assert_invalid(db.read(), NativeField::Role);
    }

    #[test]
    fn validates_session_identity_parent_and_update_time() {
        let db = Database::new();
        db.message("assistant", "session", &assistant(CREATED + 1000));
        db.writer
            .execute("UPDATE session SET parent_id=' '", [])
            .unwrap();
        assert_invalid(db.read(), NativeField::SessionParentId);
        db.writer
            .execute("UPDATE session SET parent_id=NULL", [])
            .unwrap();
        db.writer
            .execute("UPDATE message SET time_updated='unknown'", [])
            .unwrap();
        assert_invalid(db.read(), NativeField::MessageUpdatedAt);
        assert_invalid(
            read_native_session(&db.path, "").usage,
            NativeField::SessionId,
        );
    }

    #[test]
    fn token_sum_overflow_is_unavailable() {
        let db = Database::new();
        let mut huge = assistant(CREATED + 1000);
        huge["tokens"] = json!({
            "input": i64::MAX, "output": i64::MAX, "reasoning": i64::MAX,
            "cache": {"read": i64::MAX, "write": i64::MAX}
        });
        db.message("huge", "session", &huge);
        let invalid = assert_invalid(db.read(), NativeField::Total);
        assert_eq!(invalid.reason, NativeInvalidReason::Overflow);
    }

    #[test]
    fn empty_supported_session_and_missing_session_are_different_results() {
        let db = Database::new();
        let snapshot = db.read().unwrap();
        assert!(snapshot.observations.is_empty());
        assert_invalid(
            read_native_session(&db.path, "missing").usage,
            NativeField::SessionId,
        );
    }

    #[test]
    fn missing_or_unknown_schema_is_unsupported_instead_of_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.db");
        let writer = Connection::open(&path).unwrap();
        for sql in [
            "CREATE TABLE unrelated(value TEXT)",
            "CREATE TABLE session(id TEXT)",
        ] {
            writer.execute_batch(sql).unwrap();
            assert_eq!(
                read_native_session(&path, "session").usage,
                Err(NativeSnapshotError::Unsupported(NativeUnsupported::Schema))
            );
        }
    }

    #[test]
    fn newer_session_message_rows_are_unsupported_even_when_old_tables_coexist() {
        let db = Database::new();
        db.message("old", "session", &assistant(CREATED + 1000));
        db.writer.execute_batch(
            "CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,data TEXT);
             INSERT INTO session_message VALUES('new','session','assistant',1,'private message');"
        ).unwrap();
        assert_eq!(
            db.read(),
            Err(NativeSnapshotError::Unsupported(
                NativeUnsupported::SessionMessage
            ))
        );
        db.writer.execute("DELETE FROM message", []).unwrap();
        assert_eq!(
            db.read(),
            Err(NativeSnapshotError::Unsupported(
                NativeUnsupported::SessionMessage
            ))
        );
    }

    #[test]
    fn cumulative_only_usage_is_unsupported_instead_of_empty() {
        let db = Database::new();
        db.writer
            .execute_batch(
                "ALTER TABLE session ADD COLUMN tokens_input INTEGER DEFAULT 100;
             ALTER TABLE session ADD COLUMN tokens_output INTEGER DEFAULT 40;
             ALTER TABLE session ADD COLUMN tokens_reasoning INTEGER DEFAULT 10;
             ALTER TABLE session ADD COLUMN tokens_cache_read INTEGER DEFAULT 30;
             ALTER TABLE session ADD COLUMN tokens_cache_write INTEGER DEFAULT 20;",
            )
            .unwrap();
        assert_eq!(
            db.read(),
            Err(NativeSnapshotError::Unsupported(
                NativeUnsupported::CumulativeOnly
            ))
        );
    }

    #[test]
    fn unknown_file_format_is_unsupported_and_missing_file_is_a_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-sqlite.json");
        std::fs::write(&path, r#"{"role":"assistant","tokens":{"input":10}}"#).unwrap();
        assert_eq!(
            read_native_session(&path, "session").usage,
            Err(NativeSnapshotError::Unsupported(
                NativeUnsupported::NotSqlite
            ))
        );
        let missing = dir.path().join("missing.db");
        assert_eq!(
            read_native_session(&missing, "session").usage,
            Err(NativeSnapshotError::ReadFailure)
        );
        assert!(!missing.exists());
    }

    #[test]
    fn uncommitted_wal_changes_never_look_like_authoritative_empty_usage() {
        let db = Database::new();
        db.writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        db.message("assistant", "session", &assistant(CREATED + 1000));
        db.writer.execute_batch("BEGIN IMMEDIATE; DELETE FROM message; UPDATE session SET parent_id='changed-parent';").unwrap();
        let during_write = db.read().unwrap();
        assert_eq!(during_write.observations.len(), 1);
        assert_eq!(
            during_write.parent_session_id.as_deref(),
            Some("native-parent")
        );
        db.writer.execute_batch("COMMIT;").unwrap();
        let after_commit = db.read().unwrap();
        assert!(after_commit.observations.is_empty());
        assert_eq!(
            after_commit.parent_session_id.as_deref(),
            Some("changed-parent")
        );
    }
}
