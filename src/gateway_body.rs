use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    extract::Request,
};

pub(crate) const GATEWAY_BODY_READ_DEADLINE: Duration = Duration::from_secs(60);
const MAX_DEFAULT_BODY: usize = 4 * 1024 * 1024;
const MAX_IMAGE_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatewayBodyAdmissionError {
    Timeout,
    Rejected,
}

pub(crate) async fn admit_gateway_request_body(
    request: Request,
    deadline: Duration,
) -> Result<Request, GatewayBodyAdmissionError> {
    let maximum = if request.uri().path() == "/v1/images/generations" {
        MAX_IMAGE_BODY
    } else {
        MAX_DEFAULT_BODY
    };
    let (parts, body) = request.into_parts();
    let bytes = tokio::time::timeout(deadline, to_bytes(body, maximum))
        .await
        .map_err(|_| GatewayBodyAdmissionError::Timeout)?
        .map_err(|_| GatewayBodyAdmissionError::Rejected)?;
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, time::Instant};

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
            admit_gateway_request_body(request, Duration::from_millis(35)).await,
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
                admit_gateway_request_body(exact, Duration::from_secs(2))
                    .await
                    .is_ok(),
                "{path} exact limit"
            );
            let over = Request::post(path)
                .body(Body::from(vec![b'x'; maximum + 1]))
                .expect("oversized request");
            assert!(
                matches!(
                    admit_gateway_request_body(over, Duration::from_secs(2)).await,
                    Err(GatewayBodyAdmissionError::Rejected)
                ),
                "{path} over limit"
            );
        }
    }
}
