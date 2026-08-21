use super::*;

pub(super) async fn validate_plan_directory(
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata = tokio::fs::symlink_metadata(directory).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("session archive plan directory must be a real directory".into());
    }
    Ok(())
}

pub(super) async fn build_import_plan(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
    lower_bound: i64,
    input: &mut ReadOnlyInput,
    source_seal: &SealedInput,
) -> Result<
    (SessionArchiveImportStats, Option<SealedImportPlan>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let (guard, mut connection) = create_import_plan(options.plan_directory).await?;
    create_import_plan_schema(&mut connection).await?;
    let mut transaction = connection.begin().await?;
    let mut stats = SessionArchiveImportStats::default();
    let mut source_hasher = blake3::Hasher::new();
    let mut record_count = 0_u64;
    let mut serialized_bytes = 0_u64;

    while let Some(line) = read_bounded_line(&mut input.reader, options.max_line_bytes).await? {
        source_hasher.update(&line);
        let Some((record, record_digest)) = parse_record(&line)? else {
            continue;
        };
        stats.scanned += 1;
        if !archive_record_inside_overlap(&record, lower_bound) {
            stats.before_overlap += 1;
            continue;
        }
        stats.eligible += 1;
        let matched = match match_record(db, options, &record, &record_digest).await {
            Ok(matched) => matched,
            Err(AppError::BadRequest(_)) if options.allow_unmapped => {
                stats.unmapped += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let plan_correlation = match &matched {
            SessionArchiveImportMatch::Correlated(correlation) => {
                preflight_gap_compatibility(db, options, &record, correlation.as_ref()).await?;
                stats.mapped += 1;
                stats.replayed += u64::from(correlation.replay());
                ImportPlanCorrelation::from(correlation.as_ref())
            }
            SessionArchiveImportMatch::Quarantine(target)
                if options.quarantine_unknown_identities =>
            {
                stats.quarantined += 1;
                ImportPlanCorrelation::Quarantined {
                    target: ImportPlanQuarantineTarget::from(target),
                }
            }
            SessionArchiveImportMatch::Quarantine(_) => {
                stats.unmapped += 1;
                continue;
            }
        };

        // Stage one payload at a time and transfer ownership of its buffer to
        // Bytes. This keeps request/response serialization buffers from
        // overlapping and avoids a second full-payload copy before upload.
        let request = payload_bytes(&record.request)?;
        let request_digest = request.as_deref().map(digest);
        // These are the only durable writes allowed before both seals validate.
        // Content addressing makes an orphan harmless if planning later fails.
        let request_object = match request {
            Some(body) => Some(archive.put_content(Bytes::from(body)).await?),
            None => None,
        };
        let response = payload_bytes(&record.response)?;
        let response_digest = response.as_deref().map(digest);
        let response_object = match response {
            Some(body) => Some(archive.put_content(Bytes::from(body)).await?),
            None => None,
        };
        let source_started_at = record.started_at.timestamp_millis();
        let completed_at = record.completed_at.timestamp_millis();
        let source_completed_at = Some(completed_at);
        let duration_ms = Some(completed_at - source_started_at);
        let (input_tokens, output_tokens) = archive_usage(&record);
        let plan_record = ImportPlanRecord {
            version: IMPORT_PLAN_VERSION,
            external_request_id: record.request_id.clone(),
            correlation: plan_correlation,
            record_digest,
            request_digest,
            response_digest,
            request_object,
            response_object,
            request_is_structured: structured_request(&record.request).is_some(),
            conversation_hints: conversation_hints(&record),
            client_name: first_facet(&record, "client")
                .or_else(|| metadata_string(&record, "client"))
                .map(str::to_owned),
            source_started_at,
            source_completed_at,
            source_checkpoint_ms: completed_at,
            protocol: archive_protocol(&record),
            model: nonempty(&record.requested_model)
                .or_else(|| nonempty(&record.model))
                .unwrap_or("unknown")
                .to_owned(),
            status_code: (record.status_code > 0).then_some(record.status_code),
            duration_ms,
            input_tokens,
            output_tokens,
            error_code: archive_error_code(&record),
        };
        validate_plan_record(&plan_record)?;
        let encoded = serde_json::to_vec(&plan_record)?;
        if encoded.len() > MAX_PLAN_RECORD_BYTES {
            return Err("session archive import plan record exceeds 512 KiB".into());
        }
        serialized_bytes = serialized_bytes.saturating_add(encoded.len() as u64);
        if serialized_bytes > options.max_plan_bytes {
            return Err("session archive import plan exceeds its configured size limit".into());
        }
        record_count += 1;
        sqlx::query(
            "INSERT INTO import_plan_records (sequence, source_started_at, source_checkpoint_ms, external_request_id, record_json) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(i64::try_from(record_count)?)
        .bind(plan_record.source_started_at)
        .bind(plan_record.source_checkpoint_ms)
        .bind(&plan_record.external_request_id)
        .bind(encoded)
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("session archive import plan is not unique: {error}"),
            )
        })?;
        if record_count.is_multiple_of(PLAN_SIZE_CHECK_INTERVAL)
            && plan_database_bytes(&mut transaction).await? > options.max_plan_bytes
        {
            return Err("session archive import plan exceeds its configured size limit".into());
        }
    }

    input
        .verify_seal(source_seal, source_hasher.finalize())
        .await?;
    if stats.unmapped > 0 {
        transaction.rollback().await?;
        connection.close().await?;
        return Ok((stats, None));
    }
    let header = ImportPlanHeader {
        version: IMPORT_PLAN_VERSION,
        tenant_external_id: options.tenant_external_id.to_owned(),
        cpamp_source: options.cpamp_source.to_owned(),
        archive_source: options.archive_source.to_owned(),
        source_size_bytes: source_seal.identity.size,
        source_blake3: source_seal.digest.to_hex().to_string(),
        record_count,
        quarantine_records: stats.quarantined,
        quarantine_batch_id: (stats.quarantined > 0).then(|| {
            quarantine_batch_id(
                options.tenant_external_id,
                options.archive_source,
                source_seal.digest.to_hex().as_ref(),
            )
        }),
        tenant_binding_kind: options.quarantine_tenant_binding_kind.map(str::to_owned),
        tenant_binding_proof: options.quarantine_tenant_binding_proof.map(str::to_owned),
        approved_by_service_id: options.quarantine_approved_by_service_id,
    };
    sqlx::query("INSERT INTO import_plan_metadata (singleton, header_json) VALUES (1, $1)")
        .bind(serde_json::to_vec(&header)?)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    connection.close().await?;
    let sealed = seal_import_plan(guard, record_count, options.max_plan_bytes).await?;
    Ok((stats, Some(sealed)))
}

pub(super) async fn create_import_plan(
    directory: &Path,
) -> Result<(PlanPathGuard, SqliteConnection), Box<dyn std::error::Error + Send + Sync>> {
    validate_plan_directory(directory).await?;
    let mut created = None;
    for _ in 0..8 {
        let path = directory.join(format!(
            ".mtc-session-archive-plan-{}.sqlite",
            Uuid::now_v7()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => {
                drop(file);
                created = Some(PlanPathGuard { path });
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let guard = created.ok_or("could not allocate a unique session archive plan")?;
    let connect_options = SqliteConnectOptions::new()
        .filename(&guard.path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Delete)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true);
    let connection = SqliteConnection::connect_with(&connect_options).await?;
    Ok((guard, connection))
}

pub(super) async fn create_import_plan_schema(
    connection: &mut SqliteConnection,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    sqlx::query(
        "CREATE TABLE import_plan_metadata (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), header_json BLOB NOT NULL); CREATE TABLE import_plan_records (sequence INTEGER PRIMARY KEY CHECK(sequence > 0), source_started_at INTEGER NOT NULL CHECK(source_started_at >= 0), source_checkpoint_ms INTEGER NOT NULL CHECK(source_checkpoint_ms >= source_started_at), external_request_id TEXT NOT NULL UNIQUE, record_json BLOB NOT NULL); CREATE INDEX import_plan_apply_order ON import_plan_records(source_checkpoint_ms, external_request_id, sequence)",
    )
    .execute(connection)
    .await?;
    Ok(())
}

pub(super) async fn plan_database_bytes(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<u64, sqlx::Error> {
    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&mut **transaction)
        .await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut **transaction)
        .await?;
    Ok((page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64))
}

pub(super) async fn seal_import_plan(
    guard: PlanPathGuard,
    record_count: u64,
    maximum: u64,
) -> Result<SealedImportPlan, Box<dyn std::error::Error + Send + Sync>> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&guard.path)
        .await?;
    file.sync_all().await?;
    drop(file);
    #[cfg(unix)]
    tokio::fs::set_permissions(&guard.path, std::fs::Permissions::from_mode(0o400)).await?;
    #[cfg(not(unix))]
    {
        let mut permissions = tokio::fs::metadata(&guard.path).await?.permissions();
        permissions.set_readonly(true);
        tokio::fs::set_permissions(&guard.path, permissions).await?;
    }
    let (identity, size, digest) = hash_plan_file(&guard.path).await?;
    if size > maximum {
        return Err("session archive import plan exceeds its configured size limit".into());
    }
    Ok(SealedImportPlan {
        path: guard,
        identity,
        size,
        digest,
        record_count,
    })
}

pub(super) async fn hash_plan_file(
    path: &Path,
) -> Result<(InputIdentity, u64, blake3::Hash), Box<dyn std::error::Error + Send + Sync>> {
    let mut file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
    let identity = InputIdentity::from_metadata(&file.metadata().await?);
    let path_identity = InputIdentity::from_metadata(&tokio::fs::metadata(path).await?);
    if identity != path_identity || !file.metadata().await?.is_file() {
        return Err(plan_changed_error().into());
    }
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let final_identity = InputIdentity::from_metadata(&file.metadata().await?);
    let final_path_identity = InputIdentity::from_metadata(&tokio::fs::metadata(path).await?);
    if identity != final_identity || identity != final_path_identity || size != identity.size {
        return Err(plan_changed_error().into());
    }
    Ok((identity, size, hasher.finalize()))
}

pub(super) fn plan_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "session archive import plan changed after it was sealed; no database apply is permitted",
    )
}

pub(super) async fn verify_plan_file(
    plan: &SealedImportPlan,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        let mode = tokio::fs::metadata(&plan.path.path)
            .await?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o400 {
            return Err(plan_changed_error().into());
        }
    }
    let (identity, size, digest) = hash_plan_file(&plan.path.path).await?;
    if identity != plan.identity || size != plan.size || digest != plan.digest {
        return Err(plan_changed_error().into());
    }
    Ok(())
}

pub(super) async fn open_validated_plan(
    plan: SealedImportPlan,
    options: &SessionArchiveImportOptions<'_>,
    source_seal: &SealedInput,
) -> Result<ValidatedImportPlan, Box<dyn std::error::Error + Send + Sync>> {
    verify_plan_file(&plan).await?;
    let connect_options = SqliteConnectOptions::new()
        .filename(&plan.path.path)
        .read_only(true);
    let mut connection = SqliteConnection::connect_with(&connect_options).await?;
    sqlx::query("PRAGMA query_only = ON")
        .execute(&mut connection)
        .await?;
    // Keep one read transaction from the complete validation scan through the
    // final apply read. SQLite's rollback-journal shared lock prevents another
    // SQLite writer from changing later rows after earlier rows have committed.
    // Apply therefore reads the exact snapshot that was parsed and hashed.
    sqlx::query("BEGIN").execute(&mut connection).await?;
    let header =
        validate_plan_contents(&mut connection, options, source_seal, plan.record_count).await?;
    // Hash again after SQLite parsed every row. This closes path replacement or
    // in-place mutation races before the first target database transaction.
    verify_plan_file(&plan).await?;
    #[cfg(unix)]
    tokio::fs::remove_file(&plan.path.path).await?;
    Ok(ValidatedImportPlan {
        connection,
        record_count: plan.record_count,
        header,
        _path: plan.path,
    })
}

pub(super) async fn validate_plan_contents(
    connection: &mut SqliteConnection,
    options: &SessionArchiveImportOptions<'_>,
    source_seal: &SealedInput,
    expected_records: u64,
) -> Result<ImportPlanHeader, Box<dyn std::error::Error + Send + Sync>> {
    let header_bytes: Vec<u8> =
        sqlx::query_scalar("SELECT header_json FROM import_plan_metadata WHERE singleton = 1")
            .fetch_one(&mut *connection)
            .await?;
    let header: ImportPlanHeader = serde_json::from_slice(&header_bytes)?;
    if header.version != IMPORT_PLAN_VERSION
        || header.tenant_external_id != options.tenant_external_id
        || header.cpamp_source != options.cpamp_source
        || header.archive_source != options.archive_source
        || header.source_size_bytes != source_seal.identity.size
        || header.source_blake3 != source_seal.digest.to_hex().as_str()
        || header.record_count != expected_records
        || header.quarantine_records > header.record_count
        || (header.quarantine_records > 0
            && (header.quarantine_batch_id.is_none()
                || header
                    .tenant_binding_kind
                    .as_deref()
                    .is_none_or(|value| !valid_plan_text(value, 128))
                || header
                    .tenant_binding_proof
                    .as_deref()
                    .is_none_or(|value| !is_digest_hex(value))))
        || (header.quarantine_records == 0
            && (header.quarantine_batch_id.is_some()
                || header.tenant_binding_kind.is_some()
                || header.tenant_binding_proof.is_some()
                || header.approved_by_service_id.is_some()))
    {
        return Err(plan_changed_error().into());
    }

    let mut count = 0_u64;
    let mut rows = sqlx::query(
        "SELECT sequence, source_started_at, source_checkpoint_ms, external_request_id, record_json FROM import_plan_records ORDER BY sequence ASC",
    )
    .fetch(&mut *connection);
    while let Some(row) = rows.try_next().await? {
        count += 1;
        let sequence: i64 = row.try_get("sequence")?;
        if sequence != i64::try_from(count)? {
            return Err(plan_changed_error().into());
        }
        let bytes: Vec<u8> = row.try_get("record_json")?;
        if bytes.len() > MAX_PLAN_RECORD_BYTES {
            return Err(plan_changed_error().into());
        }
        let record: ImportPlanRecord = serde_json::from_slice(&bytes)?;
        validate_plan_record(&record)?;
        if row.try_get::<i64, _>("source_started_at")? != record.source_started_at
            || row.try_get::<i64, _>("source_checkpoint_ms")? != record.source_checkpoint_ms
            || row.try_get::<String, _>("external_request_id")? != record.external_request_id
        {
            return Err(plan_changed_error().into());
        }
    }
    if count != expected_records {
        return Err(plan_changed_error().into());
    }
    Ok(header)
}

pub(super) fn validate_plan_record(record: &ImportPlanRecord) -> Result<(), io::Error> {
    let correlation_valid = match &record.correlation {
        ImportPlanCorrelation::Exact {
            target,
            identity_proof_kind,
            identity_proof_digest,
            correlation_proof_digest,
        } => {
            target.tenant_id == target.key.tenant_id
                && !target.target_request_id.is_nil()
                && is_digest_hex(&target.external_event_hash)
                && valid_plan_text(&target.source_model, 512)
                && valid_plan_text(identity_proof_kind, 200)
                && is_digest_hex(identity_proof_digest)
                && is_digest_hex(correlation_proof_digest)
        }
        ImportPlanCorrelation::Unlinked { target } => {
            target.tenant_id == target.key.tenant_id
                && !target.archive_request_id.is_nil()
                && valid_plan_text(&target.identity_proof_kind, 200)
                && is_digest_hex(&target.identity_proof_digest)
                && is_digest_hex(&target.correlation_proof_digest)
        }
        ImportPlanCorrelation::Quarantined { target } => {
            !target.tenant_id.is_nil()
                && !target.quarantine_id.is_nil()
                && matches!(
                    target.reason_code.as_str(),
                    "missing_credential_hash" | "unproven_identity"
                )
                && target
                    .identity_claim_digest
                    .as_deref()
                    .is_none_or(is_digest_hex)
                && is_digest_hex(&target.proof_digest)
        }
    };
    let timing_valid = match (record.source_completed_at, record.duration_ms) {
        (Some(completed), Some(duration)) => completed
            .checked_sub(record.source_started_at)
            .is_some_and(|expected| {
                expected >= 0 && duration == expected && record.source_checkpoint_ms == completed
            }),
        (None, None) => record.source_checkpoint_ms == record.source_started_at,
        _ => false,
    };
    if record.version != IMPORT_PLAN_VERSION
        || record.external_request_id.is_empty()
        || record.external_request_id.len() > 512
        || record.external_request_id.chars().any(char::is_control)
        || !correlation_valid
        || !is_digest_hex(&record.record_digest)
        || record
            .request_digest
            .as_deref()
            .is_some_and(|value| !is_digest_hex(value))
        || record
            .response_digest
            .as_deref()
            .is_some_and(|value| !is_digest_hex(value))
        || !plan_object_matches(
            record.request_object.as_deref(),
            record.request_digest.as_deref(),
        )
        || !plan_object_matches(
            record.response_object.as_deref(),
            record.response_digest.as_deref(),
        )
        || (record.request_is_structured && record.request_object.is_none())
        || !timing_valid
        || !valid_plan_text(&record.protocol, 512)
        || !valid_plan_text(&record.model, 512)
        || record.status_code.is_some_and(|status| status <= 0)
        || record.input_tokens < 0
        || record.output_tokens < 0
        || record
            .error_code
            .as_deref()
            .is_some_and(|value| !valid_plan_text(value, 200))
    {
        return Err(plan_changed_error());
    }
    Ok(())
}

pub(super) fn valid_plan_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

pub(super) fn plan_object_matches(location: Option<&str>, digest: Option<&str>) -> bool {
    match (location, digest) {
        (None, None) => true,
        (Some(location), Some(digest)) if is_digest_hex(digest) => {
            location == content_location(digest)
        }
        _ => false,
    }
}

pub(super) fn is_digest_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
