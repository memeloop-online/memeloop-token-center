use super::*;
use crate::db::test_support::PostgresTestDatabase;

#[tokio::test]
#[ignore = "requires dedicated PostgreSQL with 32 primary connections"]
async fn postgres_cold_oauth_authority_uses_existing_pool_and_preserves_shared_authority() {
    Box::pin(disposable_database_contract()).await;
}

async fn disposable_database_contract() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL URL required");
    let isolated = PostgresTestDatabase::create(&url).await;
    let preparation = Database::connect(isolated.url()).await.unwrap();
    preparation.migrate().await.unwrap();
    preparation.close().await;
    let directory = tempfile::tempdir().unwrap();
    let mut resources = super::oauth_fixture_contract::FixtureResources::default();
    Box::pin(super::oauth_fixture_contract::run_contract(
        isolated.url(),
        directory.path(),
        "cold-oauth-fixture",
        None,
        &mut resources,
    ))
    .await;
    isolated.cleanup().await;
}
