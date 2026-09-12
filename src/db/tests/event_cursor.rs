use super::super::*;

async fn insert_event(
    transaction: &mut Transaction<'_, Any>,
    cursor: &RequestEventCursor,
    tenant_id: Uuid,
    key_id: Uuid,
    request_id: Uuid,
) {
    sqlx::query(
        "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', 'openai', 'cursor-contract', 0, 0, 0)",
    )
    .bind(&cursor.event_id)
    .bind(tenant_id.to_string())
    .bind(key_id.to_string())
    .bind(request_id.to_string())
    .bind(cursor.event_at)
    .execute(&mut **transaction)
    .await
    .unwrap();
}

async fn allocate_fixture(
    database: &Database,
    now: i64,
    tenant_id: Uuid,
    key_id: Uuid,
    request_id: Uuid,
) -> (Transaction<'static, Any>, RequestEventCursor) {
    let mut transaction = database.begin_write_transaction().await.unwrap();
    let cursor = allocate_request_event_cursor(
        &mut transaction,
        now,
        &tenant_id.to_string(),
        &key_id.to_string(),
        &request_id.to_string(),
    )
    .await
    .unwrap();
    insert_event(&mut transaction, &cursor, tenant_id, key_id, request_id).await;
    (transaction, cursor)
}

async fn wait_for_postgres_advisory_lock(
    observer: &Database,
    application_name: &str,
    backend_pid: i32,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                   SELECT 1 FROM pg_stat_activity activity \
                   JOIN pg_locks advisory ON advisory.pid = activity.pid \
                   WHERE activity.pid = $1 AND activity.application_name = $2 \
                     AND activity.state = 'active' \
                     AND activity.wait_event_type = 'Lock' \
                     AND activity.wait_event = 'advisory' \
                     AND advisory.locktype = 'advisory' AND NOT advisory.granted\
                 )",
            )
            .bind(backend_pid)
            .bind(application_name)
            .fetch_one(&observer.pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second allocator did not reach the PostgreSQL advisory lock");
}

#[tokio::test]
async fn sqlite_global_cursor_uses_covering_index_across_database_instances_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("event-cursor.db").display()
    );
    let first = Database::connect(&database_url).await.unwrap();
    first.migrate().await.unwrap();
    let second = Database::connect(&database_url).await.unwrap();

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT event_at, event_id FROM request_events ORDER BY event_at DESC, event_id DESC LIMIT 1",
    )
    .fetch_all(&first.pool)
    .await
    .unwrap();
    assert!(plan.iter().any(|row| {
        row.try_get::<String, _>("detail")
            .is_ok_and(|detail| detail.contains("request_events_global_cursor_idx"))
    }));

    let tenant_id = Uuid::now_v7();
    let key_id = Uuid::now_v7();
    let fixed_now = unix_millis();

    let seed_request = Uuid::now_v7();
    let (seed_transaction, seed_cursor) =
        allocate_fixture(&first, fixed_now, tenant_id, key_id, seed_request).await;
    seed_transaction.commit().await.unwrap();

    let rolled_back_request = Uuid::now_v7();
    let mut rolled_back = first.begin_write_transaction().await.unwrap();
    let rolled_back_cursor = allocate_request_event_cursor(
        &mut rolled_back,
        fixed_now,
        &tenant_id.to_string(),
        &key_id.to_string(),
        &rolled_back_request.to_string(),
    )
    .await
    .unwrap();
    let inserted = sqlx::query(
        "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) \
         SELECT $1, tenant_id, key_id, id, $2, 'finished', protocol, model, 0, 0, 0 \
         FROM request_records WHERE id = $3",
    )
    .bind(&rolled_back_cursor.event_id)
    .bind(rolled_back_cursor.event_at)
    .bind(rolled_back_request.to_string())
    .execute(&mut *rolled_back)
    .await
    .unwrap();
    assert_eq!(inserted.rows_affected(), 0);
    rolled_back.rollback().await.unwrap();
    let rolled_back_count: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM request_events WHERE event_id = $1) + (SELECT COUNT(*) FROM request_event_locators WHERE id = $1)",
    )
    .bind(&rolled_back_cursor.event_id)
    .fetch_one(&second.pool)
    .await
    .unwrap();
    assert_eq!(rolled_back_count, 0);

    let first_request = Uuid::now_v7();
    let (first_transaction, first_cursor) =
        allocate_fixture(&first, fixed_now, tenant_id, key_id, first_request).await;
    first_transaction.commit().await.unwrap();
    assert_eq!(first_cursor, rolled_back_cursor);
    assert!(
        (first_cursor.event_at, &first_cursor.event_id)
            > (seed_cursor.event_at, &seed_cursor.event_id)
    );
    let seed_uuid = Uuid::parse_str(&seed_cursor.event_id).unwrap();
    let first_uuid = Uuid::parse_str(&first_cursor.event_id).unwrap();
    assert_eq!(first_cursor.event_at, seed_cursor.event_at);
    assert_eq!(&first_uuid.as_bytes()[..8], &seed_uuid.as_bytes()[..8]);
    assert_eq!(first_uuid.get_version_num(), 7);
    assert_eq!(first_uuid.get_variant(), uuid::Variant::RFC4122);
    let second_request = Uuid::now_v7();
    let (second_transaction, second_cursor) =
        allocate_fixture(&second, fixed_now, tenant_id, key_id, second_request).await;
    second_transaction.commit().await.unwrap();
    assert!(
        (second_cursor.event_at, &second_cursor.event_id)
            > (first_cursor.event_at, &first_cursor.event_id)
    );
}

#[tokio::test]
async fn postgres_global_cursor_lock_orders_commit_visibility() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let first = Database::connect(&database_url).await.unwrap();
    first.migrate().await.unwrap();
    let second = Database::connect(&database_url).await.unwrap();
    let observer = Database::connect(&database_url).await.unwrap();
    let tenant_id = Uuid::now_v7();
    let tenant_external_id = format!("request-event-cursor-{}", Uuid::now_v7());
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
        .bind(tenant_id.to_string())
        .bind(&tenant_external_id)
        .bind(unix_millis())
        .execute(&observer.pool)
        .await
        .unwrap();
    let key_id = Uuid::now_v7();
    let first_request = Uuid::now_v7();
    let fixed_now = unix_millis();

    let (first_transaction, first_cursor) =
        allocate_fixture(&first, fixed_now, tenant_id, key_id, first_request).await;
    let second_request = Uuid::now_v7();
    let second_application_name = format!("request-event-cursor-{}", Uuid::now_v7());
    let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
    let mut second_task = tokio::spawn(async move {
        let mut transaction = second.begin_write_transaction().await.unwrap();
        sqlx::query("SELECT set_config('application_name', $1, true)")
            .bind(&second_application_name)
            .execute(&mut *transaction)
            .await
            .unwrap();
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        pid_sender
            .send((backend_pid, second_application_name))
            .unwrap();
        let cursor = allocate_request_event_cursor(
            &mut transaction,
            fixed_now,
            &tenant_id.to_string(),
            &key_id.to_string(),
            &second_request.to_string(),
        )
        .await
        .unwrap();
        insert_event(&mut transaction, &cursor, tenant_id, key_id, second_request).await;
        transaction.commit().await.unwrap();
        cursor
    });
    let (second_backend_pid, second_application_name) = pid_receiver.await.unwrap();
    wait_for_postgres_advisory_lock(&observer, &second_application_name, second_backend_pid).await;
    let visible_before_commit = observer
        .request_events_after(&tenant_external_id, 0, None, 500)
        .await
        .unwrap();
    assert!(visible_before_commit.is_empty());

    first_transaction.commit().await.unwrap();
    let second_cursor = (&mut second_task).await.unwrap();
    assert!(
        (second_cursor.event_at, &second_cursor.event_id)
            > (first_cursor.event_at, &first_cursor.event_id)
    );
    let after_first = Uuid::parse_str(&first_cursor.event_id).unwrap();
    let resumed = observer
        .request_events_after(
            &tenant_external_id,
            first_cursor.event_at,
            Some(after_first),
            500,
        )
        .await
        .unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].event_id.to_string(), second_cursor.event_id);
}
