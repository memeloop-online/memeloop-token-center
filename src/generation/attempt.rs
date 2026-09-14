use super::*;
use crate::api::{MediaAttemptGuard, MediaAttemptTerminal};
use crate::{db::UpstreamFailureKind, metrics::UpstreamHealthReason};

/// Owns one HTTP attempt, with the generation job's durable ownership fence
/// in addition to the existing account health lease.
pub(super) struct Attempt<'a> {
    state: &'a AppState,
    worker: &'a str,
    job: &'a GenerationJobWork,
    route: &'a ResolvedUpstream,
    guard: Option<MediaAttemptGuard>,
    terminal: MediaAttemptTerminal,
    pub(super) valid: bool,
}

impl<'a> Attempt<'a> {
    pub(super) fn new(
        state: &'a AppState,
        worker: &'a str,
        job: &'a GenerationJobWork,
        route: &'a ResolvedUpstream,
    ) -> Self {
        Self {
            state,
            worker,
            job,
            route,
            guard: None,
            terminal: MediaAttemptTerminal::Inconclusive,
            valid: false,
        }
    }

    pub(super) async fn admit(&mut self) -> Result<(), AppError> {
        if self.guard.is_none() {
            self.guard = Some(
                super::group_routing::admit(
                    self.state,
                    self.job.tenant_id,
                    self.job.job_id,
                    self.route,
                )
                .await?,
            );
        }
        Ok(())
    }

    pub(super) fn transport_error(&mut self, error: &reqwest::Error) {
        // A timeout after POST transmission is delivery-unknown, not permission
        // to resend or evidence that the supplier circuit has failed.
        if error.is_connect() {
            self.terminal = MediaAttemptTerminal::Failed {
                kind: UpstreamFailureKind::Connection,
                reason: UpstreamHealthReason::Connection,
            };
        }
    }

    pub(super) fn envelope_valid(&mut self) {
        self.terminal = MediaAttemptTerminal::Inconclusive;
    }

    pub(super) fn invalid_response(&mut self) {
        self.terminal = MediaAttemptTerminal::invalid_response();
    }

    pub(super) async fn json(&mut self, response: Response) -> Result<Value, AppError> {
        let (parsed, terminal) = classified_json(response).await;
        self.terminal = terminal;
        parsed
    }

    /// A multi-HTTP poll may advance only after valid intermediate evidence
    /// and a fresh durable job ownership CAS, never by sharing a probe lease.
    pub(super) async fn next_http(&mut self) -> Result<(), AppError> {
        if let Err(error) = self
            .state
            .db
            .renew_generation_lease(self.job.job_id, self.worker)
            .await
        {
            if let Some(mut guard) = self.guard.take() {
                guard.abandon_without_observe().await;
            }
            return Err(error);
        }
        if let Some(mut guard) = self.guard.take() {
            guard.complete(MediaAttemptTerminal::Succeeded).await;
        }
        self.valid = false;
        self.terminal = MediaAttemptTerminal::Inconclusive;
        self.admit().await
    }

    pub(super) async fn finish(mut self, result: Result<(), AppError>) -> Result<(), AppError> {
        // Keep the guard in self across the ownership check so cancellation
        // uses our no-observation Drop, not the proxy guard's normal Drop.
        let lost = self.guard.is_some()
            && result.is_err()
            && self
                .state
                .db
                .renew_generation_lease(self.job.job_id, self.worker)
                .await
                .is_err();
        if let Some(mut guard) = self.guard.take() {
            if lost {
                guard.abandon_without_observe().await;
            } else {
                let terminal = if result.is_ok() && self.valid {
                    MediaAttemptTerminal::Succeeded
                } else if self.valid {
                    MediaAttemptTerminal::Inconclusive
                } else {
                    self.terminal
                };
                guard.complete(terminal).await;
            }
        }
        result
    }
}

async fn classified_json(response: Response) -> (Result<Value, AppError>, MediaAttemptTerminal) {
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let kind = crate::api::classify_media_rate_limit(response).await;
        let terminal = MediaAttemptTerminal::Failed {
            kind,
            reason: UpstreamHealthReason::RateLimited,
        };
        return (Ok(serde_json::json!({})), terminal);
    }
    let mut terminal = match status.as_u16() {
        401 | 403 => MediaAttemptTerminal::Failed {
            kind: UpstreamFailureKind::Authentication,
            reason: UpstreamHealthReason::Unavailable,
        },
        500..=599 => MediaAttemptTerminal::Failed {
            kind: UpstreamFailureKind::Unavailable,
            reason: UpstreamHealthReason::Unavailable,
        },
        _ => MediaAttemptTerminal::Inconclusive,
    };
    let parsed = bounded_json(response).await;
    if status.is_success() {
        terminal = MediaAttemptTerminal::invalid_response();
    }
    (parsed, terminal)
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        // Outer job-lease loss cancels the HTTP future. The account lease must
        // be released, but a stale worker cannot publish a cancelled hook.
        if let Some(mut guard) = self.guard.take() {
            tokio::spawn(async move {
                guard.abandon_without_observe().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    async fn response(status: u16, body: &str) -> Response {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "120")
                    .set_body_raw(body.to_owned(), "application/json"),
            )
            .mount(&server)
            .await;
        // Buffer the body while the server is alive, then reconstruct a real
        // reqwest response so the classifier exercises its bounded body path.
        let response = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap();
        let mut response = http::Response::builder().status(status).body(body).unwrap();
        *response.headers_mut() = headers;
        response.into()
    }

    #[tokio::test]
    async fn authentication_is_preserved_even_when_error_envelope_is_invalid() {
        for status in [401, 403] {
            let (body, terminal) = classified_json(response(status, "invalid json").await).await;
            assert!(body.is_err());
            assert!(matches!(
                terminal,
                MediaAttemptTerminal::Failed {
                    kind: UpstreamFailureKind::Authentication,
                    ..
                }
            ));
        }
        let (_, terminal) = classified_json(response(400, "{}").await).await;
        assert!(matches!(terminal, MediaAttemptTerminal::Inconclusive));
    }

    #[tokio::test]
    async fn rate_limit_uses_existing_retry_after_parser_not_transient_policy() {
        let before = unix_millis();
        let (_, terminal) = classified_json(response(429, "{}").await).await;
        assert!(
            matches!(terminal, MediaAttemptTerminal::Failed { kind: UpstreamFailureKind::RateLimitedUntil { until, .. }, .. } if until >= before + 120_000)
        );
    }

    #[tokio::test]
    async fn successful_headers_or_json_alone_never_heal_a_probe() {
        let (body, terminal) = classified_json(response(200, "{}").await).await;
        assert!(body.is_ok());
        assert!(matches!(
            terminal,
            MediaAttemptTerminal::Failed {
                kind: UpstreamFailureKind::InvalidResponse,
                ..
            }
        ));
        let (body, terminal) = classified_json(response(200, "invalid json").await).await;
        assert!(body.is_err());
        assert!(matches!(
            terminal,
            MediaAttemptTerminal::Failed {
                kind: UpstreamFailureKind::InvalidResponse,
                ..
            }
        ));
        let (_, terminal) = classified_json(response(503, "{}").await).await;
        assert!(matches!(
            terminal,
            MediaAttemptTerminal::Failed {
                kind: UpstreamFailureKind::Unavailable,
                ..
            }
        ));
    }
}
