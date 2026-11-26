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
        || !cluster_path.starts_with("/api/v1/namespaces/")
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
    let database_url = std::env::var("DATABASE_URL")?;
    let pepper = std::env::var("KEY_PEPPER")?;
    if pepper.is_empty() {
        return Err("missing credential sealing material".into());
    }
    let mut guard = sqlx::PgConnection::connect(&database_url).await?;
    let identity = sqlx::query("SELECT current_database() AS db, current_user AS role, (SELECT MAX(version) FROM schema_migrations) AS version")
        .fetch_one(&mut guard).await?;
    if identity.try_get::<String, _>("db")? != database
        || identity.try_get::<String, _>("role")? != role
        || identity.try_get::<i64, _>("version")? != 166
    {
        return Err("database identity or schema mismatch".into());
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
    db.upsert_model_price("fixture-model", "USD", Decimal::ZERO, Decimal::ZERO)
        .await?;
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
