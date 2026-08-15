use std::path::Path;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    archive::ArchiveStore,
    conversation::ConversationHints,
    db::{Database, SessionArchiveCommitInput, SessionArchiveMatchInput, SessionArchiveTarget},
    error::AppError,
};

#[derive(Clone, Debug)]
pub struct SessionArchiveImportOptions<'a> {
    pub input: &'a Path,
    pub tenant_external_id: &'a str,
    pub cpamp_source: &'a str,
    pub archive_source: &'a str,
    pub overlap_ms: i64,
    pub time_tolerance_ms: i64,
    pub max_line_bytes: usize,
    pub allow_unmapped: bool,
    pub apply: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionArchiveImportStats {
    pub scanned: u64,
    pub eligible: u64,
    pub before_overlap: u64,
    pub mapped: u64,
    pub unmapped: u64,
    pub replayed: u64,
    pub imported: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArchiveRecord {
    schema_version: i64,
    session_id: String,
    request_id: String,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    #[serde(default)]
    key_id: String,
    #[serde(default)]
    principal_id: String,
    #[serde(default)]
    credential_hash: String,
    #[serde(default)]
    requested_model: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    status_code: i64,
    #[serde(default)]
    metadata: serde_json::Map<String, Value>,
    #[serde(default)]
    facets: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    request: Value,
    #[serde(default)]
    response: Value,
}

pub async fn import_session_archive(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
) -> Result<SessionArchiveImportStats, Box<dyn std::error::Error + Send + Sync>> {
    validate_name(options.tenant_external_id, "tenant external id")?;
    validate_name(options.cpamp_source, "CPAMP source")?;
    validate_name(options.archive_source, "archive source")?;
    if options.max_line_bytes < 1024 || options.max_line_bytes > 256 * 1024 * 1024 {
        return Err("max line bytes must be between 1 KiB and 256 MiB".into());
    }
    let lower_bound = db
        .session_archive_lower_bound(
            options.tenant_external_id,
            options.archive_source,
            options.overlap_ms,
        )
        .await?;

    // Pass one never writes. A missing/ambiguous CPAMP identity or a protected
    // request/response locator therefore stops the entire batch before any target
    // object or relational row can be changed.
    let mut stats = SessionArchiveImportStats::default();
    let mut reader = open_reader(options.input).await?;
    while let Some(line) = read_bounded_line(&mut reader, options.max_line_bytes).await? {
        let Some((record, digest)) = parse_record(&line)? else {
            continue;
        };
        stats.scanned += 1;
        let started_at = record.started_at.timestamp_millis();
        if started_at < lower_bound {
            stats.before_overlap += 1;
            continue;
        }
        stats.eligible += 1;
        match match_record(db, options, &record, &digest).await {
            Ok(target) => {
                preflight_gap_compatibility(db, options, &record, &target).await?;
                stats.mapped += 1;
                stats.replayed += u64::from(target.replay);
            }
            Err(AppError::BadRequest(_)) if options.allow_unmapped => stats.unmapped += 1,
            Err(AppError::BadRequest(_)) => {
                stats.unmapped += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if stats.unmapped > 0 && !options.allow_unmapped {
        return Err(format!(
            "archive import stopped before writes: {} of {} eligible records were unmapped, ambiguous, or inconsistent",
            stats.unmapped, stats.eligible
        )
        .into());
    }
    if !options.apply {
        return Ok(stats);
    }

    let mut reader = open_reader(options.input).await?;
    while let Some(line) = read_bounded_line(&mut reader, options.max_line_bytes).await? {
        let Some((record, record_digest)) = parse_record(&line)? else {
            continue;
        };
        if record.started_at.timestamp_millis() < lower_bound {
            continue;
        }
        let target = match match_record(db, options, &record, &record_digest).await {
            Ok(target) => target,
            Err(AppError::BadRequest(_)) if options.allow_unmapped => continue,
            Err(error) => return Err(error.into()),
        };
        if target.replay {
            continue;
        }

        let request = payload_bytes(&record.request)?;
        let response = payload_bytes(&record.response)?;
        let request_digest = request.as_ref().map(|body| digest(body));
        let response_digest = response.as_ref().map(|body| digest(body));
        // CAS writes precede the transaction. A crash can leave only unreferenced,
        // content-addressed objects; replay writes the same names and is harmless.
        let request_object = match request.as_ref() {
            Some(body) => Some(archive.put_content(Bytes::copy_from_slice(body)).await?),
            None => None,
        };
        let response_object = match response.as_ref() {
            Some(body) => Some(archive.put_content(Bytes::copy_from_slice(body)).await?),
            None => None,
        };
        let hints = conversation_hints(&record);
        let client = first_facet(&record, "client").or_else(|| metadata_string(&record, "client"));
        let request_json = structured_request(&record.request);
        let imported = db
            .commit_session_archive_request(SessionArchiveCommitInput {
                tenant_external_id: options.tenant_external_id,
                archive_source: options.archive_source,
                external_request_id: &record.request_id,
                target: &target,
                record_digest: &record_digest,
                request_digest: request_digest.as_deref(),
                response_digest: response_digest.as_deref(),
                request_object: request_object.as_deref(),
                response_object: response_object.as_deref(),
                request_json,
                conversation_hints: &hints,
                client_name: client,
                source_started_at: record.started_at.timestamp_millis(),
            })
            .await?;
        stats.imported += u64::from(imported);
    }
    Ok(stats)
}

async fn match_record(
    db: &Database,
    options: &SessionArchiveImportOptions<'_>,
    record: &ArchiveRecord,
    record_digest: &str,
) -> Result<SessionArchiveTarget, AppError> {
    db.match_session_archive_request(SessionArchiveMatchInput {
        tenant_external_id: options.tenant_external_id,
        cpamp_source: options.cpamp_source,
        archive_source: options.archive_source,
        external_request_id: &record.request_id,
        started_at: record.started_at.timestamp_millis(),
        requested_model: nonempty(&record.requested_model),
        resolved_model: nonempty(&record.model),
        credential_hash: nonempty(&record.credential_hash),
        legacy_key_id: nonempty(&record.key_id),
        record_digest,
        time_tolerance_ms: options.time_tolerance_ms,
    })
    .await
}

async fn preflight_gap_compatibility(
    db: &Database,
    options: &SessionArchiveImportOptions<'_>,
    record: &ArchiveRecord,
    target: &SessionArchiveTarget,
) -> Result<(), AppError> {
    if target.replay {
        return Ok(());
    }

    let request_object = payload_content_location(&record.request)?;
    let response_object = payload_content_location(&record.response)?;
    let current = db
        .request_archive_refs_for_tenant(options.tenant_external_id, target.target_request_id)
        .await?;
    gap_compatible(&current.request_object, request_object.as_deref())?;
    if let Some(current_response) = current.response_object.as_deref() {
        gap_compatible(current_response, response_object.as_deref())?;
    }
    Ok(())
}

fn payload_content_location(value: &Value) -> Result<Option<String>, AppError> {
    payload_bytes(value)
        .map(|body| body.map(|body| content_location(&digest(&body))))
        .map_err(|_| AppError::Internal)
}

fn content_location(digest: &str) -> String {
    format!("objects/blake3/{}/{digest}", &digest[..2])
}

fn gap_compatible(current: &str, replacement: Option<&str>) -> Result<(), AppError> {
    if replacement.is_none() || current.starts_with("gap://") || Some(current) == replacement {
        return Ok(());
    }
    Err(AppError::BadRequest(
        "archive import refused to overwrite an existing object".into(),
    ))
}

fn parse_record(
    line: &[u8],
) -> Result<Option<(ArchiveRecord, String)>, Box<dyn std::error::Error + Send + Sync>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let record: ArchiveRecord = serde_json::from_slice(line)?;
    if record.schema_version != 2 {
        return Err(format!(
            "unsupported cpa-session-archive schema {}",
            record.schema_version
        )
        .into());
    }
    if record.request_id.is_empty()
        || record.request_id.len() > 512
        || record.request_id.chars().any(char::is_control)
        || record.session_id.len() > 1024
        || record.session_id.chars().any(char::is_control)
    {
        return Err("archive record contains an invalid request or session id".into());
    }
    let canonical = serde_json::to_vec(&record)?;
    Ok(Some((record, digest(&canonical))))
}

fn payload_bytes(value: &Value) -> Result<Option<Vec<u8>>, serde_json::Error> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.as_bytes().to_vec())),
        value => serde_json::to_vec(value).map(Some),
    }
}

fn structured_request(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(_) | Value::Object(_) => Some(value),
        _ => None,
    }
}

fn conversation_hints(record: &ArchiveRecord) -> ConversationHints {
    let turn_id = first_facet(record, "turn.id").map(str::to_owned);
    let parent_turn_id = first_facet(record, "parent.turn.id")
        .or_else(|| metadata_string(record, "parent_response_id"))
        .map(str::to_owned);
    let branch_id = first_facet(record, "branch.id").map(str::to_owned);
    let compaction = record
        .facets
        .get("request.kind")
        .is_some_and(|values| values.iter().any(|value| value.contains("compaction")));
    ConversationHints {
        session_id: nonempty(&record.session_id).map(str::to_owned),
        turn_id,
        parent_turn_id,
        branch_id,
        compaction,
    }
}

fn first_facet<'a>(record: &'a ArchiveRecord, name: &str) -> Option<&'a str> {
    record
        .facets
        .get(name)
        .and_then(|values| values.iter().find_map(|value| nonempty(value)))
}

fn metadata_string<'a>(record: &'a ArchiveRecord, name: &str) -> Option<&'a str> {
    record
        .metadata
        .get(name)
        .and_then(Value::as_str)
        .and_then(nonempty)
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn validate_name(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(format!("{label} contains unsupported characters"));
    }
    Ok(())
}

async fn open_reader(
    path: &Path,
) -> Result<BufReader<tokio::fs::File>, Box<dyn std::error::Error + Send + Sync>> {
    let file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
    Ok(BufReader::new(file))
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::fs::File>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > maximum {
            return Err(format!("archive JSONL record exceeds {maximum} bytes").into());
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_strings_restore_raw_bytes_and_objects_restore_json() {
        assert_eq!(
            payload_bytes(&Value::String("raw".into())).unwrap(),
            Some(b"raw".to_vec())
        );
        assert_eq!(
            payload_bytes(&serde_json::json!({"a": 1})).unwrap(),
            Some(br#"{"a":1}"#.to_vec())
        );
        assert_eq!(payload_bytes(&Value::Null).unwrap(), None);
    }

    #[test]
    fn invalid_sources_are_rejected() {
        assert!(validate_name("archive-v2", "source").is_ok());
        assert!(validate_name("../archive", "source").is_err());
    }
}
