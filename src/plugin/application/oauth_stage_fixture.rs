//! Explicit Stage-only existing-database runner. All target identity reads
//! precede writes; this module cannot provision, migrate or drop a database.
use super::oauth_fixture_contract::{FixtureResources, file_hash, run_contract};
use super::*;
use futures_util::FutureExt;
use sqlx::{Connection, Row};
use std::{net::IpAddr, os::unix::fs::PermissionsExt, panic::AssertUnwindSafe};

type GuardResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn pin(value: Option<&'static str>) -> GuardResult<&'static str> {
    value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "missing build-time target pin".into())
}
struct Target {
    guard: sqlx::PgConnection,
    database_url: String,
    pepper: String,
    work_root: PathBuf,
    source: &'static str,
}
async fn preflight() -> GuardResult<Target> {
    if std::env::var("MTC_STAGE_FIXTURE_ARMED").as_deref() != Ok("existing-database-once") {
        return Err("explicit existing-database fixture arm is missing".into());
    }
    let database = pin(option_env!("MTC_STAGE_FIXTURE_DATABASE"))?;
    let role = pin(option_env!("MTC_STAGE_FIXTURE_DATABASE_ROLE"))?;
    let cluster_uid = pin(option_env!("MTC_STAGE_FIXTURE_CLUSTER_UID"))?;
    // This path identifies the cluster identity object verified by the Stage operator.
    let cluster_path = pin(option_env!("MTC_STAGE_FIXTURE_CLUSTER_IDENTITY_PATH"))?;
    let namespace = pin(option_env!("MTC_STAGE_FIXTURE_NAMESPACE"))?;
    let namespace_uid = pin(option_env!("MTC_STAGE_FIXTURE_NAMESPACE_UID"))?;
    let pg_host = pin(option_env!("MTC_STAGE_FIXTURE_PG_HOST"))?;
    let pg_port: u16 = pin(option_env!("MTC_STAGE_FIXTURE_PG_PORT"))?.parse()?;
    let pg_service_uid = pin(option_env!("MTC_STAGE_FIXTURE_PG_SERVICE_UID"))?;
    // Only a same-namespace Service's short DNS name is accepted, never an arbitrary host.
    if pg_host.is_empty()
        || !pg_host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("invalid PostgreSQL service pin".into());
    }
    if cluster_path != "/api/v1/namespaces/kube-system" || namespace.contains('/') {
        return Err("invalid Stage identity path".into());
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
    if pepper.len() < 32 {
        return Err("missing credential sealing material".into());
    }
    let mut guard = sqlx::PgConnection::connect(&database_url).await?;
    let identity = sqlx::query("SELECT current_database() AS db, current_user AS role, host(inet_server_addr()) AS server_addr, inet_server_port() AS server_port, (SELECT MAX(version) FROM schema_migrations) AS version")
        .fetch_one(&mut guard).await?;
    if identity.try_get::<String, _>("db")? != database
        || identity.try_get::<String, _>("role")? != role
        || identity.try_get::<i64, _>("version")? != 163
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
    let locked: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_lock(hashtext('stage-existing-oauth-fixture'), 163)",
    )
    .fetch_one(&mut guard)
    .await?;
    if !locked {
        return Err("fixture provisioning already active".into());
    }
    // This one dedicated session owns the lock for the whole fixture. It is
    // never returned to a pool or replaced/reconnected after a failure.
    let other_sessions:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid() AND backend_type = 'client backend'")
        .fetch_one(&mut guard).await?;
    if other_sessions != 0 {
        return Err("another application is connected to fixture database".into());
    }
    let occupied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM upstream_accounts UNION ALL SELECT 1 FROM application_plugin_head UNION ALL SELECT 1 FROM application_plugin_revisions UNION ALL SELECT 1 FROM application_plugin_operations UNION ALL SELECT 1 FROM plugin_configurations UNION ALL SELECT 1 FROM tenants WHERE external_id <> 'default' UNION ALL SELECT 1 FROM tenant_lifecycle_audit)")
        .fetch_one(&mut guard).await?;
    if occupied {
        return Err("nonempty target or prior attempt; automatic replay forbidden".into());
    }
    let version_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version=163")
            .fetch_one(&mut guard)
            .await?;
    if version_count != 1 {
        return Err("schema163 receipt mismatch".into());
    }
    let work_root = std::path::PathBuf::from(pin(option_env!("MTC_STAGE_FIXTURE_WORK_ROOT"))?);
    if !work_root.is_absolute()
        || std::fs::canonicalize(&work_root)? != work_root
        || !std::fs::symlink_metadata(&work_root)?.file_type().is_dir()
    {
        return Err("fixture work root is not an existing canonical directory".into());
    }
    let source = pin(option_env!("MTC_STAGE_FIXTURE_SOURCE"))?;
    if source.len() != 40 || !source.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("missing fixed fixture source".into());
    }
    Ok(Target {
        guard,
        database_url,
        pepper,
        work_root,
        source,
    })
}

#[tokio::test]
#[ignore = "requires compiled Stage target pins and exclusive existing schema163 database"]
async fn stage_existing_database_cold_oauth_authority() {
    Box::pin(stage_contract()).await;
}

async fn stage_contract() {
    let mut target = preflight()
        .await
        .unwrap_or_else(|_| panic!("Stage fixture preflight rejected; no fixture data written"));
    let db = Database::connect_with_max(&target.database_url, 2)
        .await
        .unwrap_or_else(|_| panic!("Stage fixture connection failed"));
    let run_id = uuid::Uuid::now_v7();
    let tenant = format!("stage-oauth-fixture-{run_id}");
    // First durable fixture write is the run identity plus normal lifecycle
    // audit. Every later partial attempt therefore fails preflight on rerun.
    db.create_tenant(&tenant, None)
        .await
        .unwrap_or_else(|_| panic!("Stage fixture attempt marker failed"));
    let directory = target.work_root.join(run_id.to_string());
    std::fs::create_dir(&directory).expect("create owned fixture directory");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .expect("restrict fixture directory");
    let attempt = json!({"source":target.source,"run_id":run_id,"tenant":tenant,"schema":163,"status":"started"});
    std::fs::write(
        directory.join("attempt.json"),
        serde_json::to_vec(&attempt).unwrap(),
    )
    .expect("write attempt receipt");
    let mut resources = FixtureResources::default();
    let outcome = AssertUnwindSafe(Box::pin(run_contract(
        &target.database_url,
        &directory,
        &tenant,
        Some(&target.pepper),
        &mut resources,
    )))
    .catch_unwind()
    .await;
    // A disconnected lock session is never replaced. Keep the attempt for
    // sole Stage operator recovery instead of doing unlocked finalization.
    let lock_alive = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
        .fetch_one(&mut target.guard)
        .await
        .is_ok();
    let finalized = if lock_alive {
        resources.finish(&db, &tenant).await
    } else {
        Err("fixture lock session lost")
    };
    let mut files = BTreeMap::new();
    let mut pending = vec![directory.clone()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).expect("owned evidence metadata");
        if metadata.file_type().is_dir() {
            for entry in std::fs::read_dir(path).expect("owned evidence directory") {
                pending.push(entry.expect("owned evidence entry").path());
            }
        } else {
            files.insert(
                path.strip_prefix(&directory)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                file_hash(&path).expect("owned regular evidence file"),
            );
        }
    }
    let receipt = json!({"source":target.source,"run_id":run_id,"tenant":tenant,"schema":163,
        "contract_passed":outcome.is_ok(),"finalized":finalized.is_ok(),"accounts_expected_updated_at":resources.accounts,
        "files_sha256":files});
    std::fs::write(
        directory.join("result.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .expect("write fixture result receipt");
    eprintln!(
        "Stage OAuth fixture run={run_id} source={} contract_passed={} finalized={}",
        target.source,
        outcome.is_ok(),
        finalized.is_ok()
    );
    assert!(
        outcome.is_ok(),
        "Stage fixture contract failed; attempt retained, do not rerun"
    );
    assert!(
        finalized.is_ok(),
        "Stage fixture finalization incomplete; preserve evidence for sole Stage operator"
    );
    db.close().await;
    target
        .guard
        .close()
        .await
        .expect("close original fixture lock session");
}
