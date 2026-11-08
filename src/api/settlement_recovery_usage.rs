//! Strict, read-only recovery parsing. No upstream calls or inference from ceilings.
use crate::model::TokenUsage;
use serde_json::Value;

fn completed(response: &Value) -> Result<TokenUsage, &'static str> {
    if response["object"] != "response"
        || response["status"] != "completed"
        || response["id"].as_str().is_none_or(str::is_empty)
        || response.get("error").is_some_and(|value| !value.is_null())
        || response
            .get("incomplete_details")
            .is_some_and(|value| !value.is_null())
    {
        return Err("response_not_authoritatively_completed");
    }
    super::proxy::codex_transport::canonical_responses_usage(response)
        .map_err(|_| "canonical_provider_usage_invalid")
}

pub(crate) fn parse(body: &[u8]) -> Result<TokenUsage, &'static str> {
    if let Ok(response) = super::sse::parse_unique_json(body) {
        return completed(&response);
    }
    let text = std::str::from_utf8(body).map_err(|_| "response_encoding_invalid")?;
    let mut data = String::new();
    let mut event_name = None;
    let mut terminal = None;
    let mut done = false;
    let mut response_id: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() {
            if !data.is_empty() {
                if done {
                    return Err("event_after_done");
                }
                if data.trim() == "[DONE]" {
                    if terminal.is_none() {
                        return Err("done_before_response_completed");
                    }
                    done = true;
                } else {
                    let event = super::sse::parse_unique_json(data.as_bytes())
                        .map_err(|_| "sse_event_invalid")?;
                    let kind = event["type"].as_str().ok_or("sse_event_type_missing")?;
                    if event_name.as_deref().is_some_and(|name| name != kind) {
                        return Err("sse_event_type_mismatch");
                    }
                    if matches!(kind, "response.failed" | "response.incomplete" | "error") {
                        return Err("response_failed_or_incomplete");
                    }
                    if let Some(id) = event["response"]["id"].as_str() {
                        if response_id
                            .as_deref()
                            .is_some_and(|expected| expected != id)
                        {
                            return Err("response_identity_conflict");
                        }
                        response_id = Some(id.to_owned());
                    }
                    if kind == "response.completed" {
                        if terminal.is_some() {
                            return Err("multiple_response_completed_events");
                        }
                        terminal = Some(completed(&event["response"])?);
                    } else if terminal.is_some() {
                        return Err("event_after_response_completed");
                    }
                }
                data.clear();
            }
            event_name = None;
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        } else if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_owned());
        } else if !line.starts_with(':') && !line.starts_with("id:") && !line.starts_with("retry:")
        {
            return Err("sse_field_invalid");
        }
    }
    if !data.is_empty() || event_name.is_some() {
        return Err("sse_terminal_frame_truncated");
    }
    terminal.ok_or("response_completed_event_missing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> String {
        format!(
            "event: response.completed\ndata: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_recovery_fixture", "object": "response", "status": "completed",
                    "service_tier": "priority",
                    "usage": {
                        "input_tokens": 108893, "output_tokens": 373, "total_tokens": 109266,
                        "input_tokens_details": {"cached_tokens": 107520}
                    }
                }
            })
        )
    }

    #[test]
    fn canonical_completed_response_preserves_cached_tokens_and_tier() {
        let usage = parse(event().as_bytes()).unwrap();
        assert_eq!(usage.input_tokens, 1373);
        assert_eq!(usage.cached_input_tokens, 107520);
        assert_eq!(usage.output_tokens, 373);
        assert_eq!(usage.total_input_tokens(), 108893);
        assert_eq!(usage.service_tier.as_deref(), Some("priority"));
        assert_eq!(
            usage.cache_coverage.unwrap().inclusive_input_tokens,
            Some(108893)
        );
    }

    #[test]
    fn truncated_duplicate_and_failed_terminal_evidence_is_rejected() {
        let complete = event();
        assert!(parse(complete.trim_end().as_bytes()).is_err());
        assert!(parse(format!("{complete}{complete}").as_bytes()).is_err());
        assert!(
            parse(
                complete
                    .replace("\"completed\"", "\"incomplete\"")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(parse(complete.replace("109266", "109267").as_bytes()).is_err());
    }

    #[test]
    fn done_must_follow_completed_and_end_the_stream() {
        let complete = event();
        assert!(parse(format!("{complete}data: [DONE]\n\n").as_bytes()).is_ok());
        assert!(parse(format!("data: [DONE]\n\n{complete}").as_bytes()).is_err());
        assert!(parse(format!("{complete}data: [DONE]\n\n{complete}").as_bytes()).is_err());
    }

    #[test]
    fn earlier_usage_is_not_authority_without_completed_terminal() {
        let body = event().replace("response.completed", "response.in_progress");
        assert_eq!(
            parse(body.as_bytes()),
            Err("response_completed_event_missing")
        );
    }

    #[test]
    fn duplicate_json_fields_and_cross_response_streams_are_rejected() {
        let duplicate = event().replace(
            "\"input_tokens\":108893",
            "\"input_tokens\":1,\"input_tokens\":108893",
        );
        assert!(parse(duplicate.as_bytes()).is_err());
        let other = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"other\"}}\n\n";
        assert_eq!(
            parse(format!("{other}{}", event()).as_bytes()),
            Err("response_identity_conflict")
        );
    }
}
