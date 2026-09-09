use super::*;

#[tokio::test]
async fn sqlite_event_enrichment_preserves_recorded_fields_and_tenant_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("events.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    assert_event_enrichment(&database).await;
}

#[tokio::test]
async fn postgres_event_enrichment_preserves_recorded_fields_and_tenant_ownership() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    assert_event_enrichment(&database).await;
}

async fn assert_event_enrichment(database: &Database) {
    let tenant = Uuid::now_v7().to_string();
    let foreign_tenant = Uuid::now_v7().to_string();
    let external = format!("event-fields-{tenant}");
    let foreign_external = format!("event-fields-{foreign_tenant}");
    let key = Uuid::now_v7().to_string();
    let request = Uuid::now_v7().to_string();
    let upstream = Uuid::now_v7();
    let route = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query(
        "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3), ($4, $5, $3)",
    )
    .bind(&tenant)
    .bind(&external)
    .bind(now)
    .bind(&foreign_tenant)
    .bind(&foreign_external)
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, $2, $3, $4)")
        .bind(&request).bind(now).bind(&tenant).bind(&key)
        .execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, completed_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cost_micros, currency, request_object, reservation_id, upstream_account_id, model_route_id, conversation_cluster_id) VALUES ($1, $2, $3, $4, $5, 'openai', 'event-model', 200, 25, 100, 20, 30, 10, 1000000, 'USD', 'gap://fixture', $6, $7, $8, $9)")
        .bind(&request).bind(&tenant).bind(&key).bind(now).bind(now + 25)
        .bind(Uuid::now_v7().to_string()).bind(upstream.to_string()).bind(route.to_string()).bind("recorded-session")
        .execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, atom_hashes_json, created_at, inference_version, session_name, task_kind, agent_id, metadata_source) VALUES ($1, 'recorded-session', $2, $3, '[]', $4, 1, 'recorded-name', 'task', 'agent', 'declared')")
        .bind(Uuid::now_v7().to_string()).bind(&request).bind(&key).bind(now)
        .execute(&database.pool).await.unwrap();
    // The foreign event deliberately references the same request/key. It must
    // never inherit the first tenant's routing or conversation fields.
    for event_tenant in [&tenant, &foreign_tenant] {
        sqlx::query("INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'finished', 'openai', 'event-model', 200, 25, 100, 20, 1000000)")
            .bind(Uuid::now_v7().to_string()).bind(event_tenant).bind(&key).bind(&request).bind(now + 50)
            .execute(&database.pool).await.unwrap();
    }
    let events = database
        .request_events_after(&external, now, None, 500)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.created_at, Some(now));
    assert_eq!(event.completed_at, Some(now + 25));
    assert_eq!(event.event_at, now + 50);
    assert_eq!(event.upstream_account_id, Some(upstream));
    assert_eq!(event.route_id, Some(route));
    assert_eq!(event.currency.as_deref(), Some("USD"));
    assert_eq!(event.cached_input_tokens, Some(30));
    assert_eq!(event.cache_write_tokens, Some(10));
    let context = event.session_context.as_ref().unwrap();
    assert_eq!(context.session_id.as_deref(), Some("recorded-session"));
    assert_eq!(context.session_name.as_deref(), Some("recorded-name"));
    let foreign = database
        .request_events_after(&foreign_external, now, None, 500)
        .await
        .unwrap();
    assert_eq!(foreign.len(), 1);
    assert!(foreign[0].created_at.is_none());
    assert!(foreign[0].upstream_account_id.is_none());
    assert!(foreign[0].route_id.is_none());
    assert!(foreign[0].currency.is_none());
    assert!(foreign[0].cached_input_tokens.is_none());
    assert!(foreign[0].session_context.is_none());
    // The SQLite fixture owns its pool. PostgreSQL CI shares its database
    // with concurrent high-volume tests, so a global first page is not ours.
    if matches!(database.backend, DatabaseBackend::Sqlite) {
        let global = database
            .all_request_events_after(now, None, 500)
            .await
            .unwrap();
        assert_eq!(global.len(), 2);
        assert_eq!(
            global
                .iter()
                .filter(|event| event.upstream_account_id == Some(upstream))
                .count(),
            1
        );
    }
}
