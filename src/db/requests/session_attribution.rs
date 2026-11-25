//! One interpretation of persisted ingress hints for every request read surface.
use crate::{conversation::ConversationHints, model::CapturedRequestSessionIdentity};

pub(super) fn captured_identity(
    hints: &ConversationHints,
) -> Option<CapturedRequestSessionIdentity> {
    let source = hints.labels.get("mtc_session_identity_source");
    let thread_source = source.is_some_and(|source| {
        matches!(
            source.as_str(),
            "header:thread-id"
                | "body:client_metadata.thread_id"
                | "body:metadata.thread_id"
                | "body:thread_id"
        )
    });
    let declared_source = source.is_some_and(|source| {
        matches!(
            source.as_str(),
            "header:x-mtc-conversation-id"
                | "compatibility_session_header"
                | "compatibility_session_body"
        )
    });
    let thread_id = thread_source.then(|| hints.session_id.clone()).flatten();
    let declared_id = declared_source.then(|| hints.session_id.clone()).flatten();
    let client_session_id = hints.labels.get("mtc_client_session_id").cloned();
    let parent_thread_id = hints.labels.get("mtc_parent_thread_id").cloned();
    // A legacy undifferentiated session hint is not evidence of a thread.
    if source.is_none() && client_session_id.is_none() && parent_thread_id.is_none() {
        return None;
    }
    Some(CapturedRequestSessionIdentity {
        thread_id,
        declared_id,
        client_session_id,
        parent_thread_id,
        source: source.cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_identity_preserves_provenance_without_confirming_shared_sessions() {
        let mut hints = ConversationHints::default();
        hints.session_id = Some("thread-a".into());
        assert!(captured_identity(&hints).is_none());
        hints.labels.insert(
            "mtc_session_identity_source".into(),
            "header:thread-id".into(),
        );
        hints
            .labels
            .insert("mtc_client_session_id".into(), "shared".into());
        let first = captured_identity(&hints).unwrap();
        assert_eq!(first.thread_id.as_deref(), Some("thread-a"));
        assert_eq!(first.client_session_id.as_deref(), Some("shared"));
        assert!(first.declared_id.is_none());
        hints.session_id = Some("thread-b".into());
        let second = captured_identity(&hints).unwrap();
        assert_ne!(first.thread_id, second.thread_id);
        assert_eq!(first.client_session_id, second.client_session_id);
        hints.labels.insert(
            "mtc_session_identity_source".into(),
            "body:prompt_cache_key".into(),
        );
        let cache = captured_identity(&hints).unwrap();
        assert!(cache.thread_id.is_none() && cache.declared_id.is_none());
        hints.labels.insert(
            "mtc_session_identity_source".into(),
            "compatibility_session_header".into(),
        );
        let declared = captured_identity(&hints).unwrap();
        assert!(declared.thread_id.is_none());
        assert_eq!(declared.declared_id.as_deref(), Some("thread-b"));
    }

    #[test]
    fn legacy_session_context_round_trip_preserves_frozen_payload_shape() {
        let old = serde_json::json!({"session_id":null,"association":"unlinked","session_name":null,"task_kind":null,"agent_id":null,"semantics_source":null});
        let context: crate::model::RequestSessionContext =
            serde_json::from_value(old.clone()).unwrap();
        assert!(context.captured.is_none());
        assert_eq!(serde_json::to_value(context).unwrap(), old);
    }
}
