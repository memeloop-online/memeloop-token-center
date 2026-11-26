//! Explicitly built, target-pinned fixture provisioning tool. Never starts a role or calls a supplier.
use memeloop_token_center::{
    db::{
        CreateKeyInput, CreateRoutedModelRouteInput, CreateUpstreamAccountInput, Database,
        DiscoveredUpstreamModel,
    },
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{Connection, Row};
use std::net::IpAddr;
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, time::Duration};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const TENANT: &str = "oauth-role-fixture";
const OUTPUT: &str = "/dev/shm/mtc-oauth-fixture.json";
fn pin(value: Option<&'static str>) -> Result<&'static str> {
    value
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing build-time target pin".into())
}

async fn seed() -> Result<()> {
    let database = pin(option_env!("MTC_SEED_DATABASE"))?;
    let role = pin(option_env!("MTC_SEED_DATABASE_ROLE"))?;
    let mock = pin(option_env!("MTC_SEED_MOCK_URL"))?;
    let cluster_uid = pin(option_env!("MTC_SEED_CLUSTER_UID"))?;
    // This path identifies the cluster identity object verified by the Stage operator.
    let cluster_path = pin(option_env!("MTC_SEED_CLUSTER_IDENTITY_PATH"))?;
    let namespace = pin(option_env!("MTC_SEED_NAMESPACE"))?;
    let namespace_uid = pin(option_env!("MTC_SEED_NAMESPACE_UID"))?;
    let pg_host = pin(option_env!("MTC_SEED_PG_HOST"))?;
    let pg_port: u16 = pin(option_env!("MTC_SEED_PG_PORT"))?.parse()?;
    let pg_service_uid = pin(option_env!("MTC_SEED_PG_SERVICE_UID"))?;
    // Only a same-namespace Service's short DNS name is accepted, never an arbitrary host.
    if pg_host.is_empty()
        || !pg_host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("invalid PostgreSQL service pin".into());
    }
    let mock_url = url::Url::parse(mock)?;
    if mock_url.scheme() != "http"
        || !mock_url.username().is_empty()
        || mock_url.password().is_some()
        || mock_url.query().is_some()
        || mock_url.fragment().is_some()
        || mock_url.path() != "/"
        || !mock_url
            .host_str()
            .is_some_and(|s| s.ends_with("-oauth-fixture"))
        || !(cluster_path.starts_with("/api/") || cluster_path.starts_with("/apis/"))
        || cluster_path.contains('?')
        || namespace.contains('/')
    {
        return Err("invalid isolated target pins".into());
    }
    let sa = "/var/run/secrets/kubernetes.io/serviceaccount";
    let mounted_namespace = std::fs::read_to_string(format!("{sa}/namespace"))?;
    if mounted_namespace.trim() != namespace {
        return Err("namespace mismatch".into());
    }
    let token = std::fs::read_to_string(format!("{sa}/token"))?;
    let ca = reqwest::Certificate::from_pem(&std::fs::read(format!("{sa}/ca.crt"))?)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(ca)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    for (path, expected) in [
        (cluster_path.to_owned(), cluster_uid),
        (format!("/api/v1/namespaces/{namespace}"), namespace_uid),
    ] {
        let identity: serde_json::Value = client
            .get(format!("https://kubernetes.default.svc{path}"))
            .bearer_auth(token.trim())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if identity.pointer("/metadata/uid").and_then(|v| v.as_str()) != Some(expected) {
            return Err("live Stage identity mismatch".into());
        }
    }
    let pg_service: serde_json::Value = client
        .get(format!(
            "https://kubernetes.default.svc/api/v1/namespaces/{namespace}/services/{pg_host}"
        ))
        .bearer_auth(token.trim())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if pg_service.pointer("/metadata/uid").and_then(|v| v.as_str()) != Some(pg_service_uid)
        || pg_service.pointer("/spec/type").and_then(|v| v.as_str()) == Some("ExternalName")
        || !pg_service
            .pointer("/spec/ports")
            .and_then(|v| v.as_array())
            .is_some_and(|ports| {
                ports.iter().any(|p| {
                    p["port"].as_u64() == Some(u64::from(pg_port))
                        && p["protocol"].as_str() == Some("TCP")
                })
            })
    {
        return Err("live PostgreSQL Service mismatch".into());
    }
    let endpoints: serde_json::Value = client
        .get(format!(
            "https://kubernetes.default.svc/api/v1/namespaces/{namespace}/endpoints/{pg_host}"
        ))
        .bearer_auth(token.trim())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let database_url = std::env::var("DATABASE_URL")?;
    let parsed = url::Url::parse(&database_url)?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql")
        || parsed.host_str() != Some(pg_host)
        || parsed.port().unwrap_or(5432) != pg_port
        || parsed.path() != format!("/{database}")
        || parsed.username() != role
        || parsed.fragment().is_some()
        || parsed.query_pairs().any(|(name, value)| {
            name != "sslmode"
                || !matches!(
                    value.as_ref(),
                    "disable" | "allow" | "prefer" | "require" | "verify-ca" | "verify-full"
                )
        })
        || [
            "PGHOST",
            "PGHOSTADDR",
            "PGPORT",
            "PGDATABASE",
            "PGUSER",
            "PGOPTIONS",
            "PGSERVICE",
            "PGSERVICEFILE",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err("database URL differs from pinned Service or has connection overrides".into());
    }
    let pepper = std::env::var("KEY_PEPPER")?;
    if pepper.is_empty() {
        return Err("missing credential sealing material".into());
    }
    let mut guard = sqlx::PgConnection::connect(&database_url).await?;
    let identity = sqlx::query("SELECT current_database() AS db, current_user AS role, host(inet_server_addr()) AS server_addr, inet_server_port() AS server_port, (SELECT MAX(version) FROM schema_migrations) AS version")
        .fetch_one(&mut guard).await?;
    if identity.try_get::<String, _>("db")? != database
        || identity.try_get::<String, _>("role")? != role
        || identity.try_get::<i64, _>("version")? != 166
    {
        return Err("database identity or schema mismatch".into());
    }
    let server_addr: IpAddr = identity.try_get::<String, _>("server_addr")?.parse()?;
    let server_port: i32 = identity.try_get("server_port")?;
    let endpoint_matches = endpoints
        .get("subsets")
        .and_then(|v| v.as_array())
        .is_some_and(|subsets| {
            subsets.iter().any(|subset| {
                subset["addresses"].as_array().is_some_and(|addresses| {
                    addresses.iter().any(|address| {
                        address["ip"]
                            .as_str()
                            .and_then(|ip| ip.parse::<IpAddr>().ok())
                            == Some(server_addr)
                    })
                }) && subset["ports"].as_array().is_some_and(|ports| {
                    ports.iter().any(|port| {
                        port["port"].as_i64() == Some(i64::from(server_port))
                            && port["protocol"].as_str() == Some("TCP")
                    })
                })
            })
        });
    if !endpoint_matches {
        return Err("connected PostgreSQL server is not a ready Stage Service endpoint".into());
    }
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext('managed-oauth-fixture'), 166)")
            .fetch_one(&mut guard)
            .await?;
    if !locked {
        return Err("fixture provisioning already active".into());
    }
    let db = Database::connect_with_max(&database_url, 2).await?;
    if !db.list_upstream_accounts(TENANT).await?.is_empty() {
        return Err("fixture accounts already exist; refusing duplicate or partial replay".into());
    }
    // create_new prevents overwriting a prior result; /dev/shm keeps the issued key off disk.
    // On any partial failure this marker remains. Operators inspect existing IDs instead of replaying.
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(OUTPUT)?;
    ensure_fixture_price(&mut guard).await?;
    let mut ids = Vec::new();
    for (name, driver) in [("gateway", "http-json"), ("control", "google-antigravity")] {
        let account = db.create_upstream_account(CreateUpstreamAccountInput {
            tenant_external_id: TENANT.into(), name: name.into(), driver: driver.into(),
            config: json!({"base_url":mock,"control_url":mock,"project_id":"fixture","network_scope":"private"}),
            credential: UpstreamCredential::OAuth {
                access_token: format!("old-{name}"), refresh_token: Some(format!("refresh-{name}")),
                expires_at: Some(memeloop_token_center::db::unix_millis()+86_400_000),
                header:"authorization".into(), prefix:"Bearer ".into(), adapter_state:None,
                proxy_url:None, proxy_network_scope:None,
            }, oauth_session_id:Some(Uuid::now_v7()), oauth_driver:Some("cursor".into()),
            oauth_refresh_url:Some(format!("{mock}/refresh-{name}")),
        }, pepper.as_bytes()).await?;
        ids.push(account.id);
    }
    let key = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: TENANT.into(),
                principal_external_id: "fixture".into(),
                alias: "fixture".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["fixture-model".into()],
                    ..Default::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper.as_bytes(),
        )
        .await?;
    let lease = Uuid::now_v7();
    if !db
        .claim_upstream_model_catalog_sync(ids[0], TENANT, 1, lease)
        .await?
    {
        return Err("catalog claim unavailable".into());
    }
    db.replace_upstream_model_catalog(
        ids[0],
        TENANT,
        1,
        lease,
        "openai_v1",
        &[DiscoveredUpstreamModel {
            model_id: "fixture-model".into(),
            protocol: "openai".into(),
            context_window: None,
            reservation_token_bound: None,
            reservation_bound_source: None,
        }],
    )
    .await?;
    db.create_routed_model_route(CreateRoutedModelRouteInput {
        tenant_external_id: TENANT.into(),
        public_model: "fixture-model".into(),
        upstream_model: "fixture-model".into(),
        protocol: "openai".into(),
        priority: 0,
        enabled: true,
        upstream_account_ids: vec![ids[0]],
        included_provider_group_ids: vec![],
        excluded_provider_group_ids: vec![],
        route_group_ids: vec![],
        route_group_names: vec![],
        granted_credential_ids: vec![key.key_id],
        custom_model_confirmed: false,
    })
    .await?;
    let result = json!({"tenant_external_id":TENANT,"gateway_account_id":ids[0],"control_account_id":ids[1],"key_id":key.key_id,"key":key.key});
    output.write_all(serde_json::to_string(&result)?.as_bytes())?;
    output.flush()?;
    println!(
        "fixture ready: gateway={} control={}; credentials in private RAM output",
        ids[0], ids[1]
    );
    Ok(())
}
// Insert-only provisioning: conflicting prices survive unchanged, including concurrent writers.
// Both inserts roll back together if either existing row has different monetary values.
async fn ensure_fixture_price(connection: &mut sqlx::PgConnection) -> Result<()> {
    let mut tx = connection.begin().await?;
    let now = memeloop_token_center::db::unix_millis();
    sqlx::query("INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, 'fixture-model', 'USD', 0, 0, 'manual', $2) ON CONFLICT(model, currency) DO NOTHING")
        .bind(Uuid::now_v7().to_string()).bind(now).execute(&mut *tx).await?;
    let base = sqlx::query("SELECT input_micros_per_million, output_micros_per_million FROM model_prices WHERE model='fixture-model' AND currency='USD' FOR UPDATE")
        .fetch_one(&mut *tx).await?;
    if base.try_get::<i64, _>("input_micros_per_million")? != 0
        || base.try_get::<i64, _>("output_micros_per_million")? != 0
    {
        return Err("existing fixture price differs; unchanged".into());
    }
    sqlx::query("INSERT INTO model_price_tiers (id, model, currency, service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source, updated_at, cache_price_estimated) VALUES ($1, 'fixture-model', 'USD', 'default', 0, 0, 0, 0, 'manual', $2, 1) ON CONFLICT(model, currency, service_tier) DO NOTHING")
        .bind(Uuid::now_v7().to_string()).bind(now).execute(&mut *tx).await?;
    let tier = sqlx::query("SELECT input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million FROM model_price_tiers WHERE model='fixture-model' AND currency='USD' AND service_tier='default' FOR UPDATE")
        .fetch_one(&mut *tx).await?;
    for column in [
        "input_micros_per_million",
        "cached_input_micros_per_million",
        "cache_write_micros_per_million",
        "output_micros_per_million",
    ] {
        if tier.try_get::<i64, _>(column)? != 0 {
            return Err("existing fixture tier differs; unchanged".into());
        }
    }
    tx.commit().await?;
    Ok(())
}
#[tokio::main]
async fn main() {
    if seed().await.is_err() {
        // Errors may contain connection details; never emit underlying credentials or URLs.
        eprintln!(
            "fixture provisioning stopped; inspect target bindings and partial fixture state before retry"
        );
        std::process::exit(1);
    }
}
