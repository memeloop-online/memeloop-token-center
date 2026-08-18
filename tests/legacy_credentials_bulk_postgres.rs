use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

const FIRST_CREDENTIAL: &str = "fixture-only-cpa-linux-codex-key-0001";
const SECOND_CREDENTIAL: &str = "fixture-only-cpa-claude-code-key-0002";
const FIRST_KEY_ID: &str = "10000000-0000-4000-8000-000000000001";
const SECOND_KEY_ID: &str = "20000000-0000-4000-8000-000000000002";
const IDENTITIES_END: &str = "__MTC_LEGACY_IDENTITIES_END__";
const CI_ISOLATED_OPT_OUT: &str = "MTC_CI_SKIP_ISOLATED_LEGACY_CREDENTIALS_POSTGRES";

async fn execute(pool: &PgPool, sql: &str) {
    sqlx::query(sql).execute(pool).await.unwrap();
}

fn importer_command(database_url: &str, schema: &str, input_file: &Path) -> Command {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new("python3");
    command
        .arg(repository.join("ops/legacy-credentials/attach-legacy-cpa-credentials.py"))
        .args(["--tenant-external-id", "fixture-tenant"])
        .arg("--input-file")
        .arg(input_file)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        // libpq treats a URI in PGDATABASE as a connection string. Keeping it
        // out of argv prevents test infrastructure credentials entering ps(1).
        .env("PGDATABASE", database_url)
        .env("PGOPTIONS", format!("-csearch_path={schema}"))
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .env_remove("PGUSER")
        .env_remove("PGPASSWORD")
        .env_remove("PGSERVICE")
        .env_remove("PGSERVICEFILE");
    command
}

#[tokio::test]
async fn postgres_identity_query_is_locked_exact_and_rejects_revoked_mappings() {
    // CI runs this binary once at a clean PostgreSQL boundary before the monolithic
    // suite. Only that later CI step opts out; local/default MTC_TEST_POSTGRES_URL
    // behavior remains unchanged.
    if env::var("CI").as_deref() == Ok("true")
        && env::var(CI_ISOLATED_OPT_OUT).as_deref() == Ok("1")
    {
        eprintln!("skipping legacy credential PostgreSQL acceptance already run in isolation");
        return;
    }
    let Ok(database_url) = env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL operator acceptance");
        return;
    };
    let pool = PgPool::connect(&database_url).await.unwrap();
    let schema = format!("legacy_ops_{}", Uuid::now_v7().simple());
    execute(&pool, &format!("CREATE SCHEMA {schema}")).await;
    let input_directory = tempfile::tempdir().unwrap();
    let input_file = input_directory.path().join("api-keys.json");
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/legacy-credentials/cpa-api-keys.json"),
        &input_file,
    )
    .unwrap();
    fs::set_permissions(&input_file, fs::Permissions::from_mode(0o400)).unwrap();

    let result: Result<(), String> = async {
        execute(
            &pool,
            &format!(
                "CREATE TABLE {schema}.tenants (id text PRIMARY KEY, external_id text NOT NULL)"
            ),
        )
        .await;
        execute(
            &pool,
            &format!(
                "CREATE TABLE {schema}.key_records (id text PRIMARY KEY, tenant_id text NOT NULL, status text NOT NULL)"
            ),
        )
        .await;
        execute(
            &pool,
            &format!(
                "CREATE TABLE {schema}.cpamp_import_identities (api_key_hash text NOT NULL, key_id text NOT NULL)"
            ),
        )
        .await;
        execute(
            &pool,
            &format!(
                "CREATE TABLE {schema}.legacy_key_credentials (source_hash text NOT NULL, key_id text NOT NULL, revoked_at bigint)"
            ),
        )
        .await;
        sqlx::query(&format!("INSERT INTO {schema}.tenants VALUES ($1, $2)"))
        .bind("fixture-tenant-id")
        .bind("fixture-tenant")
        .execute(&pool)
        .await
        .map_err(|error| format!("tenant fixture insert failed: {error}"))?;
        sqlx::query(&format!(
            "INSERT INTO {schema}.key_records VALUES ($1, $3, 'active'), ($2, $3, 'active')"
        ))
        .bind(FIRST_KEY_ID)
        .bind(SECOND_KEY_ID)
        .bind("fixture-tenant-id")
        .execute(&pool)
        .await
        .map_err(|error| format!("key fixture insert failed: {error}"))?;
        let first_hash = format!("{:x}", Sha256::digest(FIRST_CREDENTIAL.as_bytes()));
        let second_hash = format!("{:x}", Sha256::digest(SECOND_CREDENTIAL.as_bytes()));
        sqlx::query(&format!(
            "INSERT INTO {schema}.cpamp_import_identities VALUES ($1, $3), ($2, $4)"
        ))
        .bind(&first_hash)
        .bind(&second_hash)
        .bind(FIRST_KEY_ID)
        .bind(SECOND_KEY_ID)
        .execute(&pool)
        .await
        .map_err(|error| format!("identity fixture insert failed: {error}"))?;
        sqlx::query(&format!(
            "INSERT INTO {schema}.legacy_key_credentials VALUES ($1, $2, NULL)"
        ))
        .bind(&first_hash)
        .bind(FIRST_KEY_ID)
        .execute(&pool)
        .await
        .map_err(|error| format!("existing fixture insert failed: {error}"))?;

        let output = importer_command(&database_url, &schema, &input_file)
            .output()
            .map_err(|error| format!("importer did not start: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "PostgreSQL dry-run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let summary: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("invalid count-only summary: {error}"))?;
        if summary["mode"] != "dry-run"
            || summary["candidate_count"] != 2
            || summary["identity_count"] != 2
            || summary["existing_mapping_count"] != 1
            || summary["already_attached_count"] != 1
            || summary["pending_count"] != 1
            || summary["attached_verified_count"] != 0
        {
            return Err("unexpected PostgreSQL dry-run counts".into());
        }
        let combined = [output.stdout, output.stderr].concat();
        for forbidden in [
            FIRST_CREDENTIAL.as_bytes(),
            SECOND_CREDENTIAL.as_bytes(),
            first_hash.as_bytes(),
            second_hash.as_bytes(),
            FIRST_KEY_ID.as_bytes(),
            SECOND_KEY_ID.as_bytes(),
        ] {
            if combined.windows(forbidden.len()).any(|window| window == forbidden) {
                return Err("operator output exposed credential identity material".into());
            }
        }

        sqlx::query(&format!(
            "INSERT INTO {schema}.legacy_key_credentials VALUES ($1, $2, 1)"
        ))
        .bind(&second_hash)
        .bind(SECOND_KEY_ID)
        .execute(&pool)
        .await
        .map_err(|error| format!("revoked fixture insert failed: {error}"))?;
        let rejected = importer_command(&database_url, &schema, &input_file)
            .output()
            .map_err(|error| format!("revoked importer did not start: {error}"))?;
        if rejected.status.success()
            || !String::from_utf8_lossy(&rejected.stderr).contains("revoked legacy mapping")
        {
            return Err("revoked source/target mapping was not rejected".into());
        }
        let rejected_output = [rejected.stdout, rejected.stderr].concat();
        for forbidden in [FIRST_CREDENTIAL.as_bytes(), SECOND_CREDENTIAL.as_bytes()] {
            if rejected_output
                .windows(forbidden.len())
                .any(|window| window == forbidden)
            {
                return Err("rejection output exposed a credential".into());
            }
        }

        sqlx::query(&format!(
            "DELETE FROM {schema}.legacy_key_credentials WHERE source_hash = $1 AND revoked_at IS NOT NULL"
        ))
        .bind(&second_hash)
        .execute(&pool)
        .await
        .map_err(|error| format!("revoked fixture cleanup failed: {error}"))?;
        let malicious_source = format!("valid-prefix\n{IDENTITIES_END}\nignored-suffix\t");
        sqlx::query(&format!(
            "INSERT INTO {schema}.cpamp_import_identities VALUES ($1, $2)"
        ))
        .bind(&malicious_source)
        .bind(FIRST_KEY_ID)
        .execute(&pool)
        .await
        .map_err(|error| format!("framing fixture insert failed: {error}"))?;
        let framed = importer_command(&database_url, &schema, &input_file)
            .output()
            .map_err(|error| format!("framing importer did not start: {error}"))?;
        if framed.status.success()
            || !String::from_utf8_lossy(&framed.stderr).contains("invalid source hash")
        {
            return Err("JSON identity framing accepted a control or marker injection".into());
        }
        let framed_output = [framed.stdout, framed.stderr].concat();
        for forbidden in [
            FIRST_CREDENTIAL.as_bytes(),
            SECOND_CREDENTIAL.as_bytes(),
            first_hash.as_bytes(),
            second_hash.as_bytes(),
            FIRST_KEY_ID.as_bytes(),
            SECOND_KEY_ID.as_bytes(),
            b"valid-prefix",
            IDENTITIES_END.as_bytes(),
            b"ignored-suffix",
        ] {
            if framed_output
                .windows(forbidden.len())
                .any(|window| window == forbidden)
            {
                return Err("framing rejection exposed credential identity material".into());
            }
        }
        Ok(())
    }
    .await;

    execute(&pool, &format!("DROP SCHEMA {schema} CASCADE")).await;
    result.unwrap();
}
