use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use super::opencode::{NativeSessionSnapshot, NativeSnapshotError, NativeUnsupported};
use super::{TokenCounts, bucket_ts};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::error::GitAiError;
use crate::metrics::{EventAttributes, MetricEvent, PosEncoded, TokenUsageValues};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS enterprise_native_sources (
    session_id TEXT NOT NULL,
    stream_path TEXT NOT NULL,
    snapshot_json TEXT,
    unavailable_reason TEXT,
    PRIMARY KEY (session_id, stream_path)
);
CREATE TABLE IF NOT EXISTS enterprise_native_sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
    projection_json TEXT NOT NULL,
    unavailable_reason TEXT
);
CREATE TABLE IF NOT EXISTS enterprise_token_parents (
    session_id TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    PRIMARY KEY (session_id, parent_id)
);";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnavailableReason {
    UnsupportedSchema,
    NotSqlite,
    SessionMessage,
    CumulativeOnly,
    InvalidObservation,
    ReadFailure,
    ConflictingSources,
    CounterOverflow,
}

impl From<&NativeSnapshotError> for UnavailableReason {
    fn from(error: &NativeSnapshotError) -> Self {
        match error {
            NativeSnapshotError::Unsupported(NativeUnsupported::Schema) => Self::UnsupportedSchema,
            NativeSnapshotError::Unsupported(NativeUnsupported::NotSqlite) => Self::NotSqlite,
            NativeSnapshotError::Unsupported(NativeUnsupported::SessionMessage) => {
                Self::SessionMessage
            }
            NativeSnapshotError::Unsupported(NativeUnsupported::CumulativeOnly) => {
                Self::CumulativeOnly
            }
            NativeSnapshotError::Invalid(_) => Self::InvalidObservation,
            NativeSnapshotError::ReadFailure => Self::ReadFailure,
        }
    }
}

impl UnavailableReason {
    pub(crate) fn diagnostic(self) -> &'static str {
        match self {
            Self::UnsupportedSchema => "native_usage_unavailable_unsupported_schema",
            Self::NotSqlite => "native_usage_unavailable_not_sqlite",
            Self::SessionMessage => "native_usage_unavailable_session_message",
            Self::CumulativeOnly => "native_usage_unavailable_cumulative_only",
            Self::InvalidObservation => "native_usage_unavailable_invalid_observation",
            Self::ReadFailure => "native_usage_unavailable_read_failure",
            Self::ConflictingSources => "native_usage_unavailable_conflicting_sources",
            Self::CounterOverflow => "native_usage_unavailable_counter_overflow",
        }
    }
}

#[derive(Default)]
pub(crate) struct NativeExport {
    pub events: Vec<MetricEvent>,
    pub parents: Vec<(String, String)>,
    pub unavailable: Vec<UnavailableReason>,
}

pub(crate) struct NativeSource<'a> {
    pub session_id: &'a str,
    pub external_session_id: &'a str,
    pub stream_path: &'a str,
    pub observed_parent_session_id: Option<&'a str>,
    pub repo_url: Option<&'a str>,
}

#[derive(Serialize, Deserialize)]
struct LocalBucket {
    model: String,
    bucket: u32,
    counts: TokenCounts,
    revision: u64,
    event: MetricEvent,
}

type BucketKey = (String, u32);

pub(crate) fn install(
    transaction: &Transaction<'_>,
    source: &NativeSource<'_>,
    result: Result<NativeSessionSnapshot, NativeSnapshotError>,
    min_bucket: u32,
    revision_floor: u64,
) -> Result<(), GitAiError> {
    let parent = source.observed_parent_session_id.or_else(|| {
        result
            .as_ref()
            .ok()
            .and_then(|snapshot| snapshot.parent_session_id.as_deref())
    });
    if let Some(parent) = parent {
        transaction.execute(
            "INSERT OR IGNORE INTO enterprise_token_parents(session_id, parent_id) VALUES(?1, ?2)",
            params![source.session_id, generate_session_id(parent, "opencode")],
        )?;
    }
    match result {
        Ok(mut snapshot) => {
            snapshot
                .observations
                .retain(|row| bucket_ts(row.timestamp_secs) >= min_bucket);
            transaction.execute(
                "INSERT INTO enterprise_native_sources(session_id, stream_path, snapshot_json)
                 VALUES(?1, ?2, ?3) ON CONFLICT(session_id, stream_path) DO UPDATE
                 SET snapshot_json=excluded.snapshot_json, unavailable_reason=NULL",
                params![
                    source.session_id,
                    source.stream_path,
                    serde_json::to_string(&snapshot)?
                ],
            )?;
        }
        Err(error) => {
            transaction.execute(
                "INSERT INTO enterprise_native_sources(session_id, stream_path, unavailable_reason)
                 VALUES(?1, ?2, ?3) ON CONFLICT(session_id, stream_path) DO UPDATE
                 SET unavailable_reason=excluded.unavailable_reason",
                params![
                    source.session_id,
                    source.stream_path,
                    serde_json::to_string(&UnavailableReason::from(&error))?
                ],
            )?;
        }
    }
    let previous: Option<String> = transaction
        .query_row(
            "SELECT projection_json FROM enterprise_native_sessions WHERE session_id=?1",
            [source.session_id],
            |row| row.get(0),
        )
        .optional()?;
    let previous: Vec<LocalBucket> = previous
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default();
    let aggregate = aggregate_sources(transaction, source.session_id, min_bucket)?;
    let (projection, unavailable) = match aggregate {
        Ok(aggregates) => {
            let mut previous: BTreeMap<BucketKey, LocalBucket> = previous
                .into_iter()
                .filter(|bucket| bucket.bucket >= min_bucket)
                .map(|bucket| ((bucket.model.clone(), bucket.bucket), bucket))
                .collect();
            let mut projection = Vec::new();
            for (key, counts) in aggregates {
                let old = previous.remove(&key);
                if counts.total == 0 && old.is_none() {
                    continue;
                }
                projection.push(project_bucket(source, key, counts, old, revision_floor)?);
            }
            for (key, old) in previous {
                projection.push(project_bucket(
                    source,
                    key,
                    TokenCounts::default(),
                    Some(old),
                    revision_floor,
                )?);
            }
            projection.sort_by(|a, b| (&a.model, a.bucket).cmp(&(&b.model, b.bucket)));
            (projection, None)
        }
        Err(reason) => (previous, Some(serde_json::to_string(&reason)?)),
    };
    transaction.execute(
        "INSERT INTO enterprise_native_sessions(session_id, projection_json, unavailable_reason)
         VALUES(?1, ?2, ?3) ON CONFLICT(session_id) DO UPDATE
         SET projection_json=excluded.projection_json, unavailable_reason=excluded.unavailable_reason",
        params![source.session_id, serde_json::to_string(&projection)?, unavailable],
    )?;
    Ok(())
}

fn aggregate_sources(
    transaction: &Transaction<'_>,
    session: &str,
    min_bucket: u32,
) -> Result<Result<BTreeMap<BucketKey, TokenCounts>, UnavailableReason>, GitAiError> {
    let mut statement = transaction.prepare(
        "SELECT snapshot_json, unavailable_reason FROM enterprise_native_sources WHERE session_id=?1 ORDER BY stream_path",
    )?;
    let mut messages = BTreeMap::new();
    for row in statement.query_map([session], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ))
    })? {
        let (snapshot, unavailable) = row?;
        if let Some(reason) = unavailable {
            return Ok(Err(serde_json::from_str(&reason)?));
        }
        let snapshot: NativeSessionSnapshot = serde_json::from_str(
            &snapshot.ok_or_else(|| GitAiError::Generic("missing native snapshot".into()))?,
        )?;
        for observation in snapshot.observations {
            if let Some(old) = messages.get(&observation.entry_key) {
                if old != &observation {
                    return Ok(Err(UnavailableReason::ConflictingSources));
                }
            } else {
                messages.insert(observation.entry_key.clone(), observation);
            }
        }
    }
    let mut buckets: BTreeMap<BucketKey, TokenCounts> = BTreeMap::new();
    for observation in messages
        .into_values()
        .filter(|row| bucket_ts(row.timestamp_secs) >= min_bucket)
    {
        let total = buckets
            .entry((observation.model, bucket_ts(observation.timestamp_secs)))
            .or_default();
        let counts = observation.tokens;
        for (target, value) in [
            (&mut total.input, counts.input),
            (&mut total.output, counts.output),
            (&mut total.cache_read, counts.cache_read),
            (&mut total.cache_write, counts.cache_write),
            (&mut total.total, counts.total),
        ] {
            let Some(sum) = target.checked_add(value) else {
                return Ok(Err(UnavailableReason::CounterOverflow));
            };
            *target = sum;
        }
        let Some(reasoning) = total
            .reasoning_output
            .unwrap_or(0)
            .checked_add(counts.reasoning_output.unwrap_or(0))
        else {
            return Ok(Err(UnavailableReason::CounterOverflow));
        };
        total.reasoning_output = Some(reasoning);
    }
    Ok(Ok(buckets))
}

fn project_bucket(
    source: &NativeSource<'_>,
    (model, bucket): BucketKey,
    counts: TokenCounts,
    previous: Option<LocalBucket>,
    floor: u64,
) -> Result<LocalBucket, GitAiError> {
    let revision = match previous {
        Some(old) if old.counts == counts => return Ok(old),
        Some(old) => old
            .revision
            .checked_add(1)
            .ok_or_else(|| GitAiError::Generic("native usage revision overflow".into()))?,
        None => 1,
    }
    .max(floor);
    let values = TokenUsageValues::new()
        .bucket_ts(u64::from(bucket))
        .speed(0)
        .emitted_seq(revision)
        .input_tokens(counts.input)
        .output_tokens(counts.output)
        .cache_read_tokens(counts.cache_read)
        .cache_write_tokens(counts.cache_write)
        .reasoning_output_tokens_opt(counts.reasoning_output)
        .total_tokens(counts.total);
    let mut attributes = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .session_id(source.session_id)
        .external_session_id(source.external_session_id)
        .tool("opencode")
        .model(&model);
    if let Some(repo) = source.repo_url {
        attributes = attributes.repo_url(repo);
    }
    Ok(LocalBucket {
        model,
        bucket,
        counts,
        revision,
        event: MetricEvent::new(&values, attributes.to_sparse()),
    })
}

pub(crate) fn export(transaction: &Transaction<'_>) -> Result<NativeExport, GitAiError> {
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='enterprise_native_sessions')", [], |row| row.get(0),
    )?;
    let mut export = NativeExport::default();
    if !exists {
        return Ok(export);
    }
    let mut statement = transaction.prepare("SELECT projection_json, unavailable_reason FROM enterprise_native_sessions ORDER BY session_id")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (projection, unavailable) = row?;
        if let Some(reason) = unavailable {
            export
                .unavailable
                .push(serde_json::from_str(&reason).map_err(|_| {
                    GitAiError::Generic("invalid local native availability".into())
                })?);
        } else {
            let buckets: Vec<LocalBucket> = serde_json::from_str(&projection)
                .map_err(|_| GitAiError::Generic("invalid local native Token projection".into()))?;
            export
                .events
                .extend(buckets.into_iter().map(|bucket| bucket.event));
        }
    }
    let mut statement = transaction.prepare(
        "SELECT session_id, parent_id FROM enterprise_token_parents ORDER BY session_id, parent_id",
    )?;
    export.parents = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(export)
}
