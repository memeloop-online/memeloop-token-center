use super::*;
use serde_json::json;
use std::sync::Arc;

pub(crate) fn configured_timeout(milliseconds: u64) -> HttpClient {
    let factory = Arc::new(HttpClientFactory::default());
    factory.publish(
        serde_json::from_value(json!({
            "revision": 1, "settings": {"defaults": {"request_timeout_ms": milliseconds}}
        }))
        .unwrap(),
    );
    factory.handle(HttpPurpose::Oauth)
}

#[test]
fn defaults_are_unlimited_and_units_are_explicit() {
    let document = HttpSettingsDocument::default();
    for effective in document.view().effective.values() {
        assert_eq!(effective.connect_timeout_ms.value, None);
        assert_eq!(effective.read_timeout_ms.value, None);
        assert_eq!(effective.request_timeout_ms.value, None);
        assert_eq!(effective.pool_idle_timeout_ms.value, None);
        assert_eq!(effective.request_timeout_ms.unit, "milliseconds");
        assert_eq!(
            effective.request_timeout_ms.source,
            ValueSource::SourceDefault
        );
        assert_eq!(effective.pool_max_idle_per_host.value, 8);
    }
}

#[test]
fn null_disables_and_absence_inherits_without_losing_round_trip() {
    let settings: HttpSettings = serde_json::from_value(json!({
        "defaults": {"request_timeout_ms": 1000},
        "purposes": {
            "oauth": {"request_timeout_ms": null},
            "probe": {"request_timeout_ms": 2000}
        }
    }))
    .unwrap();
    assert_eq!(
        settings
            .effective(HttpPurpose::General)
            .request_timeout_ms
            .value,
        Some(1000)
    );
    let oauth = settings.effective(HttpPurpose::Oauth).request_timeout_ms;
    assert_eq!(oauth.value, None);
    assert_eq!(oauth.source, ValueSource::PurposeOverride);
    assert_eq!(
        settings
            .effective(HttpPurpose::Probe)
            .request_timeout_ms
            .value,
        Some(2000)
    );
    let encoded = serde_json::to_value(&settings).unwrap();
    assert_eq!(
        encoded["purposes"]["oauth"]["request_timeout_ms"],
        json!(null)
    );
    assert!(
        encoded["purposes"]["oauth"]
            .get("read_timeout_ms")
            .is_none()
    );
    assert_eq!(
        serde_json::from_value::<HttpSettings>(encoded).unwrap(),
        settings
    );
}

#[test]
fn invalid_settings_fail_closed() {
    for value in [
        json!({"defaults":{"typo":1}}),
        json!({"purposes":{"unknown":{}}}),
        json!({"defaults":{"pool_max_idle_per_host":null}}),
        json!({"defaults":{"request_timeout_ms":-1}}),
        json!({"defaults":{"request_timeout_ms":1.5}}),
    ] {
        assert!(serde_json::from_value::<HttpSettings>(value).is_err());
    }
    for value in [0, 9_007_199_254_740_992_u64] {
        let settings: HttpSettings = serde_json::from_value(json!({
            "defaults":{"request_timeout_ms":value}
        }))
        .unwrap();
        assert!(settings.validate().is_err());
    }
}

#[tokio::test]
async fn updates_replace_pools_but_keep_old_clients_and_ignore_stale_refresh() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = connections.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buffer = [0; 4096];
                while socket.read(&mut buffer).await.unwrap_or(0) > 0 {
                    if socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    let factory = Arc::new(HttpClientFactory::default());
    let handle = factory.handle(HttpPurpose::General);
    let old = handle.http_client(ClientOptions::default()).unwrap();
    for _ in 0..2 {
        let client = handle.http_client(ClientOptions::default()).unwrap();
        assert_eq!(
            client.get(&url).send().await.unwrap().text().await.unwrap(),
            "ok"
        );
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    factory.publish(HttpSettingsDocument {
        revision: 1,
        ..Default::default()
    });
    let new = handle.http_client(ClientOptions::default()).unwrap();
    assert_eq!(
        new.get(&url).send().await.unwrap().text().await.unwrap(),
        "ok"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    assert_eq!(
        old.get(&url).send().await.unwrap().text().await.unwrap(),
        "ok"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    factory.publish(HttpSettingsDocument::default());
    assert_eq!(factory.snapshot().document.revision, 1);
    server.abort();
}

#[tokio::test]
async fn settings_cas_is_durable_and_refresh_publishes_a_snapshot() {
    let database = crate::db::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    database.migrate().await.unwrap();
    let settings: HttpSettings = serde_json::from_value(json!({
        "purposes":{"probe":{"request_timeout_ms":1000}}
    }))
    .unwrap();
    let document = database
        .replace_http_settings(0, settings.clone(), None)
        .await
        .unwrap();
    assert_eq!(document.revision, 1);
    assert!(matches!(
        database.replace_http_settings(0, settings, None).await,
        Err(crate::error::AppError::Conflict(_))
    ));
    database.refresh_http_settings().await.unwrap();
    assert_eq!(database.http_clients.snapshot().document, document);
}

#[tokio::test]
async fn changed_deadline_affects_new_requests_but_not_a_body_already_in_flight() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut old, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        old.read(&mut buffer).await.unwrap();
        old.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .await
            .unwrap();
        let (mut new, _) = listener.accept().await.unwrap();
        new.read(&mut buffer).await.unwrap();
        released.await.unwrap();
        old.write_all(b"ok").await.unwrap();
    });
    let factory = Arc::new(HttpClientFactory::default());
    let handle = factory.handle(HttpPurpose::General);
    let old = handle
        .http_client(ClientOptions::default())
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap();
    factory.publish(
        serde_json::from_value(json!({
            "revision": 1, "settings": {"defaults": {"request_timeout_ms": 25}}
        }))
        .unwrap(),
    );
    let error = handle
        .http_client(ClientOptions::default())
        .unwrap()
        .get(&url)
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout());
    release.send(()).unwrap();
    assert_eq!(old.text().await.unwrap(), "ok");
    server.await.unwrap();
}
