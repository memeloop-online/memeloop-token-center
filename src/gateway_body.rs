use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::header,
};

pub(crate) const GATEWAY_BODY_READ_DEADLINE: Duration = Duration::from_secs(60);
const MAX_DEFAULT_BODY: usize = 4 * 1024 * 1024;
const MAX_IMAGE_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatewayBodyAdmissionError {
    CapacityExhausted,
    Timeout,
    Rejected,
}

pub(crate) async fn admit_gateway_request_body(
    request: Request,
    deadline: Duration,
    permits: Arc<tokio::sync::Semaphore>,
) -> Result<Request, GatewayBodyAdmissionError> {
    // This guard is intentionally local to buffering. Downstream parsing,
    // routing, proxying and response streaming have their own limits, so a
    // slow lifecycle cannot turn this high-cardinality body-read admission
    // into a fixed low global request cap.
    let _body_read_permit = permits
        .try_acquire_owned()
        .map_err(|_| GatewayBodyAdmissionError::CapacityExhausted)?;
    let maximum = if request.uri().path() == "/v1/images/generations" {
        MAX_IMAGE_BODY
    } else {
        MAX_DEFAULT_BODY
    };
    admit_request_body(request, deadline, maximum).await
}

pub(crate) async fn admit_request_body(
    request: Request,
    deadline: Duration,
    maximum: usize,
) -> Result<Request, GatewayBodyAdmissionError> {
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > u64::try_from(maximum).unwrap_or(u64::MAX))
    {
        return Err(GatewayBodyAdmissionError::Rejected);
    }
    let (parts, body) = request.into_parts();
    let bytes = tokio::time::timeout(deadline, to_bytes(body, maximum))
        .await
        .map_err(|_| GatewayBodyAdmissionError::Timeout)?
        .map_err(|_| GatewayBodyAdmissionError::Rejected)?;
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Arc, time::Instant};

    use axum::http::Request;
    use bytes::Bytes;
    use futures_util::stream;

    use super::*;

    #[tokio::test]
    async fn absolute_deadline_rejects_a_drip_body() {
        let drip = stream::unfold(0_u64, |index| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Some((Ok::<_, Infallible>(Bytes::from_static(b"x")), index + 1))
        });
        let request = Request::post("/v1/chat/completions")
            .body(Body::from_stream(drip))
            .expect("drip request");
        let started = Instant::now();
        assert!(matches!(
            admit_gateway_request_body(
                request,
                Duration::from_millis(35),
                Arc::new(tokio::sync::Semaphore::new(1)),
            )
            .await,
            Err(GatewayBodyAdmissionError::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn body_limit_is_path_specific_and_exact() {
        for (path, maximum) in [
            ("/v1/chat/completions", MAX_DEFAULT_BODY),
            ("/v1/images/generations", MAX_IMAGE_BODY),
        ] {
            let exact = Request::post(path)
                .body(Body::from(vec![b'x'; maximum]))
                .expect("exact request");
            assert!(
                admit_gateway_request_body(
                    exact,
                    Duration::from_secs(2),
                    Arc::new(tokio::sync::Semaphore::new(1)),
                )
                    .await
                    .is_ok(),
                "{path} exact limit"
            );
            let over = Request::post(path)
                .body(Body::from(vec![b'x'; maximum + 1]))
                .expect("oversized request");
            assert!(
                matches!(
                    admit_gateway_request_body(
                        over,
                        Duration::from_secs(2),
                        Arc::new(tokio::sync::Semaphore::new(1)),
                    )
                    .await,
                    Err(GatewayBodyAdmissionError::Rejected)
                ),
                "{path} over limit"
            );
        }
    }

    #[tokio::test]
    async fn explicit_control_plane_limit_uses_the_same_bounded_reader() {
        let exact = Request::post("/internal/v1/imports/example")
            .body(Body::from(vec![b'x'; 257]))
            .unwrap();
        assert!(
            admit_request_body(exact, Duration::from_secs(1), 257)
                .await
                .is_ok()
        );
        let over = Request::post("/internal/v1/imports/example")
            .body(Body::from(vec![b'x'; 258]))
            .unwrap();
        assert!(matches!(
            admit_request_body(over, Duration::from_secs(1), 257).await,
            Err(GatewayBodyAdmissionError::Rejected)
        ));

        let declared_over = Request::post("/internal/v1/imports/example")
            .header(header::CONTENT_LENGTH, "258")
            .body(Body::empty())
            .unwrap();
        assert!(matches!(
            admit_request_body(declared_over, Duration::from_secs(1), 257).await,
            Err(GatewayBodyAdmissionError::Rejected)
        ));
    }

    #[tokio::test]
    async fn gateway_body_permit_is_fail_fast_and_released_after_read_completion_or_error() {
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let (started, started_wait) = tokio::sync::oneshot::channel();
        let (continue_read, continue_wait) = tokio::sync::oneshot::channel();
        let blocking_body = Body::from_stream(stream::once(async move {
            let _ = started.send(());
            let _ = continue_wait.await;
            Ok::<_, Infallible>(Bytes::from_static(b"x"))
        }));
        let request = Request::post("/v1/chat/completions")
            .body(blocking_body)
            .expect("blocking request");
        let active_permits = permits.clone();
        let read = tokio::spawn(async move {
            admit_gateway_request_body(request, Duration::from_secs(1), active_permits).await
        });

        started_wait.await.expect("reader started");
        assert_eq!(permits.available_permits(), 0);
        let rejected = Request::post("/v1/chat/completions")
            .body(Body::empty())
            .expect("second request");
        assert!(matches!(
            admit_gateway_request_body(rejected, Duration::from_secs(1), permits.clone()).await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));

        continue_read.send(()).expect("resume reader");
        assert!(read.await.expect("body read task").is_ok());
        assert_eq!(permits.available_permits(), 1);

        let failed = Request::post("/v1/chat/completions")
            .body(Body::from_stream(stream::once(async {
                Err::<Bytes, _>(std::io::Error::other("body stream failed"))
            })))
            .expect("failed request");
        assert!(matches!(
            admit_gateway_request_body(failed, Duration::from_secs(1), permits.clone()).await,
            Err(GatewayBodyAdmissionError::Rejected)
        ));
        assert_eq!(permits.available_permits(), 1);
    }
}
