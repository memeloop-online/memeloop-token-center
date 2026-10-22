use super::*;

const MAXIMUM: usize = 2 * 1024 * 1024;

fn reset_body() -> Body {
    Body::from_stream(futures_util::stream::iter([
        Ok(bytes::Bytes::from(vec![b'x'; MAXIMUM / 2])),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "private-upload-error-canary",
        )),
    ]))
}

#[tokio::test]
async fn read_failures_and_limits_are_distinct_across_spool_metered_and_fallback_readers() {
    for route_class in [
        GatewayBodyRouteClass::Responses,
        GatewayBodyRouteClass::Default,
        GatewayBodyRouteClass::Other,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
        let budget = memory::ProxyMemoryBudget::new(16 * 1024 * 1024);
        let mut cases = vec![
            (
                reset_body(),
                None,
                GatewayBodyRejectionReason::BodyReadRejected,
            ),
            (
                reset_body(),
                Some(MAXIMUM),
                GatewayBodyRejectionReason::BodyReadRejected,
            ),
            (
                Body::from(vec![b'x'; MAXIMUM + 1]),
                None,
                GatewayBodyRejectionReason::BodyLimitExceeded,
            ),
            (
                Body::empty(),
                Some(MAXIMUM + 1),
                GatewayBodyRejectionReason::DeclaredContentLengthExceedsLimit,
            ),
        ];
        if route_class != GatewayBodyRouteClass::Other {
            cases.push((
                Body::from("{}"),
                Some(1),
                GatewayBodyRejectionReason::BodyLimitExceeded,
            ));
        }
        for (body, declared, reason) in cases {
            let mut request = Request::post("/");
            if let Some(length) = declared {
                request = request.header(header::CONTENT_LENGTH, length);
            }
            let reservation =
                (route_class != GatewayBodyRouteClass::Other).then(|| budget.reservation());
            let error = admit_request_body_for_route(
                request.body(body).unwrap(),
                None,
                MAXIMUM,
                route_class,
                reservation,
                Some(&spool),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, GatewayBodyAdmissionError::Rejected(rejection)
                    if rejection.route_class == route_class
                        && rejection.reason == reason
                        && rejection.declared_content_length == declared.map(|length| length as u64)
                        && rejection.limit_bytes == MAXIMUM),
                "{route_class:?}: {error:?}"
            );
            assert_eq!(budget.snapshot().0, 0);
            assert_eq!(spool.snapshot().used_bytes, 0);
            assert_eq!(spool.snapshot().active_files, 0);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
        let admitted = admit_request_body_for_route(
            Request::post("/")
                .header(header::CONTENT_LENGTH, MAXIMUM)
                .body(Body::from(vec![b'x'; MAXIMUM]))
                .unwrap(),
            None,
            MAXIMUM,
            route_class,
            (route_class != GatewayBodyRouteClass::Other).then(|| budget.reservation()),
            Some(&spool),
        )
        .await
        .unwrap();
        drop(admitted);
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(spool.snapshot().used_bytes, 0);
        assert_eq!(spool.snapshot().active_files, 0);
    }
}

#[tokio::test]
async fn explicit_control_reader_preserves_stream_failure_classification() {
    let error = admit_request_body(
        Request::post("/").body(reset_body()).unwrap(),
        Duration::from_secs(5),
        MAXIMUM,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        GatewayBodyAdmissionError::Rejected(rejection)
            if rejection.reason == GatewayBodyRejectionReason::BodyReadRejected
    ));
}

#[test]
fn read_and_size_metrics_keep_distinct_bounded_reasons() {
    let metrics = GatewayBodyRejectionMetrics::default();
    for reason in [
        GatewayBodyRejectionReason::BodyReadRejected,
        GatewayBodyRejectionReason::BodyLimitExceeded,
    ] {
        metrics.observe(GatewayBodyRejection {
            route_class: GatewayBodyRouteClass::Responses,
            declared_content_length: None,
            limit_bytes: MAXIMUM,
            reason,
        });
    }
    let runtime = crate::metrics::RuntimeMetrics {
        gateway_body_rejections: metrics.snapshot(),
        ..Default::default()
    };
    let rendered = crate::metrics::Metrics::default().render(&runtime);
    for reason in ["body_read_rejected", "body_limit_exceeded"] {
        assert!(rendered.contains(&format!(
            "memeloop_token_center_gateway_body_rejections_total{{route_class=\"responses\",reason=\"{reason}\"}} 1"
        )));
    }
    assert!(!rendered.contains("private-upload-error-canary"));
}
