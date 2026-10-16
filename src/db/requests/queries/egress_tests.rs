use super::*;

#[tokio::test]
async fn empty_request_page_does_not_query_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("empty-page.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    assert!(
        request_list_egress(&database, &[])
            .await
            .unwrap()
            .is_empty()
    );
}
