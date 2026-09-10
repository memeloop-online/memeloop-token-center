use super::*;

#[cfg(test)]
tokio::task_local! {
    static TEST_CREDENTIAL_APPLICATION_NOW: std::cell::Cell<Option<i64>>;
}

pub(in crate::api::proxy) fn credential_application_now() -> i64 {
    #[cfg(test)]
    if let Ok(Some(now)) = TEST_CREDENTIAL_APPLICATION_NOW.try_with(|clock| clock.take()) {
        return now;
    }
    unix_millis()
}

#[cfg(test)]
pub(in crate::api::proxy) async fn with_test_credential_application_now_once<F>(
    now: i64,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    TEST_CREDENTIAL_APPLICATION_NOW
        .scope(std::cell::Cell::new(Some(now)), future)
        .await
}
