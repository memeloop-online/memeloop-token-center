use std::{
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::header,
};

pub(crate) const GATEWAY_BODY_READ_DEADLINE: Duration = Duration::from_secs(60);
const MAX_DEFAULT_BODY: usize = 4 * 1024 * 1024;
const MAX_IMAGE_BODY: usize = 16 * 1024 * 1024;
pub(crate) const GATEWAY_BODY_ROUTE_CLASS_COUNT: usize = 4;
pub(crate) const GATEWAY_BODY_REJECTION_REASON_COUNT: usize = 2;

/// Fixed route classes prevent request paths from becoming metric labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatewayBodyRouteClass {
    Default,
    Responses,
    Images,
    Other,
}

impl GatewayBodyRouteClass {
    pub(crate) const ALL: [Self; GATEWAY_BODY_ROUTE_CLASS_COUNT] = [
        Self::Default,
        Self::Responses,
        Self::Images,
        Self::Other,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Default => 0,
            Self::Responses => 1,
            Self::Images => 2,
            Self::Other => 3,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Responses => "responses",
            Self::Images => "images",
            Self::Other => "other",
        }
    }
}

/// Rejection causes are intentionally fixed and contain no request content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatewayBodyRejectionReason {
    DeclaredContentLengthExceedsLimit,
    BodyReadRejected,
}

impl GatewayBodyRejectionReason {
    pub(crate) const ALL: [Self; GATEWAY_BODY_REJECTION_REASON_COUNT] = [
        Self::DeclaredContentLengthExceedsLimit,
        Self::BodyReadRejected,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::DeclaredContentLengthExceedsLimit => 0,
            Self::BodyReadRejected => 1,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DeclaredContentLengthExceedsLimit => "declared_content_length_exceeds_limit",
            Self::BodyReadRejected => "body_read_rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GatewayBodyRejection {
    pub(crate) route_class: GatewayBodyRouteClass,
    pub(crate) declared_content_length: Option<u64>,
    pub(crate) limit_bytes: usize,
    pub(crate) reason: GatewayBodyRejectionReason,
}

/// Process-local counters rendered through `/metrics` with only fixed labels.
pub(crate) struct GatewayBodyRejectionMetrics {
    values: [[AtomicU64; GATEWAY_BODY_REJECTION_REASON_COUNT]; GATEWAY_BODY_ROUTE_CLASS_COUNT],
}

impl Default for GatewayBodyRejectionMetrics {
    fn default() -> Self {
        Self {
            values: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

impl GatewayBodyRejectionMetrics {
    pub(crate) fn observe(&self, rejection: GatewayBodyRejection) {
        self.values[rejection.route_class.index()][rejection.reason.index()]
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(
        &self,
    ) -> [[u64; GATEWAY_BODY_REJECTION_REASON_COUNT]; GATEWAY_BODY_ROUTE_CLASS_COUNT] {
        std::array::from_fn(|route| {
            std::array::from_fn(|reason| self.values[route][reason].load(Ordering::Relaxed))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatewayBodyAdmissionError {
    CapacityExhausted,
    Timeout,
    Rejected(GatewayBodyRejection),
}

pub(crate) async fn admit_gateway_request_body(
    request: Request,
    deadline: Duration,
    permits: Arc<tokio::sync::Semaphore>,
    responses_permits: Arc<tokio::sync::Semaphore>,
    responses_maximum: usize,
) -> Result<Request, GatewayBodyAdmissionError> {
    // This guard is intentionally local to buffering. Downstream parsing,
    // routing, proxying and response streaming have their own limits, so a
    // slow lifecycle cannot turn this high-cardinality body-read admission
    // into a fixed low global request cap.
    let (maximum, route_class) = gateway_body_limit(request.uri().path(), responses_maximum);
    let _responses_body_read_permit = (route_class == GatewayBodyRouteClass::Responses)
        .then(|| {
            responses_permits
                .try_acquire_owned()
                .map_err(|_| GatewayBodyAdmissionError::CapacityExhausted)
        })
        .transpose()?;
    let _body_read_permit = permits
        .try_acquire_owned()
        .map_err(|_| GatewayBodyAdmissionError::CapacityExhausted)?;
    admit_request_body_for_route(request, deadline, maximum, route_class).await
}

pub(crate) async fn admit_request_body(
    request: Request,
    deadline: Duration,
    maximum: usize,
) -> Result<Request, GatewayBodyAdmissionError> {
    admit_request_body_for_route(request, deadline, maximum, GatewayBodyRouteClass::Other).await
}

async fn admit_request_body_for_route(
    request: Request,
    deadline: Duration,
    maximum: usize,
    route_class: GatewayBodyRouteClass,
) -> Result<Request, GatewayBodyAdmissionError> {
    let declared_content_length = declared_content_length(request.headers());
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > u64::try_from(maximum).unwrap_or(u64::MAX))
    {
        return Err(rejected_body(
            route_class,
            declared_content_length,
            maximum,
            GatewayBodyRejectionReason::DeclaredContentLengthExceedsLimit,
        ));
    }
    let (parts, body) = request.into_parts();
    let bytes = tokio::time::timeout(deadline, to_bytes(body, maximum))
        .await
        .map_err(|_| GatewayBodyAdmissionError::Timeout)?
        .map_err(|_| {
            rejected_body(
                route_class,
                declared_content_length,
                maximum,
                GatewayBodyRejectionReason::BodyReadRejected,
            )
        })?;
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

fn gateway_body_limit(path: &str, responses_maximum: usize) -> (usize, GatewayBodyRouteClass) {
    match path {
        "/v1/responses" => (
            responses_maximum.min(crate::config::MAX_RESPONSES_BODY_MAX_BYTES as usize),
            GatewayBodyRouteClass::Responses,
        ),
        "/v1/images/generations" => (MAX_IMAGE_BODY, GatewayBodyRouteClass::Images),
        _ => (MAX_DEFAULT_BODY, GatewayBodyRouteClass::Default),
    }
}

fn declared_content_length(headers: &axum::http::HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn rejected_body(
    route_class: GatewayBodyRouteClass,
    declared_content_length: Option<u64>,
    limit_bytes: usize,
    reason: GatewayBodyRejectionReason,
) -> GatewayBodyAdmissionError {
    GatewayBodyAdmissionError::Rejected(GatewayBodyRejection {
        route_class,
        declared_content_length,
        limit_bytes,
        reason,
    })
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
                Arc::new(tokio::sync::Semaphore::new(1)),
                16 * 1024 * 1024,
            )
            .await,
            Err(GatewayBodyAdmissionError::Timeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn body_limit_is_path_specific_and_exact() {
        let responses_maximum = 16 * 1024 * 1024;
        for (path, maximum) in [
            ("/v1/chat/completions", MAX_DEFAULT_BODY),
            ("/v1/responses", responses_maximum),
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
                    Arc::new(tokio::sync::Semaphore::new(1)),
                    responses_maximum,
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
                        Arc::new(tokio::sync::Semaphore::new(1)),
                        responses_maximum,
                    )
                    .await,
                    Err(GatewayBodyAdmissionError::Rejected(_))
                ),
                "{path} over limit"
            );
        }
    }

    #[tokio::test]
    async fn responses_declared_limit_rejection_has_only_safe_fixed_metadata() {
        let maximum = 16 * 1024 * 1024;
        let request = Request::post("/v1/responses")
            .header(header::CONTENT_LENGTH, (maximum + 1).to_string())
            .body(Body::empty())
            .expect("declared oversized responses request");
        assert!(matches!(
            admit_gateway_request_body(
                request,
                Duration::from_secs(1),
                Arc::new(tokio::sync::Semaphore::new(1)),
                Arc::new(tokio::sync::Semaphore::new(1)),
                maximum,
            )
            .await,
            Err(GatewayBodyAdmissionError::Rejected(GatewayBodyRejection {
                route_class: GatewayBodyRouteClass::Responses,
                declared_content_length: Some(length),
                limit_bytes,
                reason: GatewayBodyRejectionReason::DeclaredContentLengthExceedsLimit,
            })) if length == u64::try_from(maximum + 1).unwrap() && limit_bytes == maximum
        ));
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
            Err(GatewayBodyAdmissionError::Rejected(_))
        ));

        let declared_over = Request::post("/internal/v1/imports/example")
            .header(header::CONTENT_LENGTH, "258")
            .body(Body::empty())
            .unwrap();
        assert!(matches!(
            admit_request_body(declared_over, Duration::from_secs(1), 257).await,
            Err(GatewayBodyAdmissionError::Rejected(_))
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
            admit_gateway_request_body(
                request,
                Duration::from_secs(1),
                active_permits,
                Arc::new(tokio::sync::Semaphore::new(1)),
                16 * 1024 * 1024,
            )
            .await
        });

        started_wait.await.expect("reader started");
        assert_eq!(permits.available_permits(), 0);
        let rejected = Request::post("/v1/chat/completions")
            .body(Body::empty())
            .expect("second request");
        assert!(matches!(
            admit_gateway_request_body(
                rejected,
                Duration::from_secs(1),
                permits.clone(),
                Arc::new(tokio::sync::Semaphore::new(1)),
                16 * 1024 * 1024,
            )
            .await,
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
            admit_gateway_request_body(
                failed,
                Duration::from_secs(1),
                permits.clone(),
                Arc::new(tokio::sync::Semaphore::new(1)),
                16 * 1024 * 1024,
            )
            .await,
            Err(GatewayBodyAdmissionError::Rejected(_))
        ));
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn responses_body_has_an_independent_bounded_admission() {
        let standard = Arc::new(tokio::sync::Semaphore::new(1));
        let responses = Arc::new(tokio::sync::Semaphore::new(1));
        let held = responses.clone().try_acquire_owned().expect("responses permit");
        let rejected = Request::post("/v1/responses")
            .body(Body::empty())
            .expect("responses request");
        assert!(matches!(
            admit_gateway_request_body(
                rejected,
                Duration::from_secs(1),
                standard.clone(),
                responses.clone(),
                16 * 1024 * 1024,
            )
            .await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
        assert_eq!(standard.available_permits(), 1);

        let ordinary = Request::post("/v1/chat/completions")
            .body(Body::empty())
            .expect("ordinary request");
        assert!(admit_gateway_request_body(
            ordinary,
            Duration::from_secs(1),
            standard.clone(),
            responses.clone(),
            16 * 1024 * 1024,
        )
        .await
        .is_ok());
        drop(held);
    }

    #[test]
    fn rejection_metrics_have_fixed_labels_and_no_request_values() {
        let metrics = GatewayBodyRejectionMetrics::default();
        metrics.observe(GatewayBodyRejection {
            route_class: GatewayBodyRouteClass::Responses,
            declared_content_length: Some(16 * 1024 * 1024 + 1),
            limit_bytes: 16 * 1024 * 1024,
            reason: GatewayBodyRejectionReason::DeclaredContentLengthExceedsLimit,
        });
        let runtime = crate::metrics::RuntimeMetrics {
            gateway_body_rejections: metrics.snapshot(),
            ..Default::default()
        };
        let rendered = crate::metrics::Metrics::default().render(&runtime);
        assert!(rendered.contains(
            "memeloop_token_center_gateway_body_rejections_total{route_class=\"responses\",reason=\"declared_content_length_exceeds_limit\"} 1"
        ));
        assert!(!rendered.contains("16777217"));
    }
}
