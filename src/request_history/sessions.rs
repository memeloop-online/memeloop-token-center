use super::*;
use crate::{
    db::{ConversationDetailFilter, LogicalSessionListFilter, RequestListFilter},
    model::*,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct SessionDirectory {
    pub key_id: uuid::Uuid,
    pub session_id: String,
    pub updated_at: i64,
    pub key_alias: String,
    pub title: Option<String>,
}

#[derive(Default)]
struct Session {
    latest: Option<RequestView>,
    latest_order: Option<(i64, uuid::Uuid)>,
    models: BTreeSet<String>,
    names: BTreeSet<String>,
    directory: Option<SessionDirectory>,
    active: i64,
    count: i64,
    errors: i64,
    input: i64,
    output: i64,
    durations: i64,
    duration_sum: i64,
    archived: i64,
    archived_errors: i64,
    archived_input: i64,
    archived_output: i64,
    archived_durations: i64,
    archived_duration_sum: i64,
    costs: BTreeMap<String, i64>,
}
impl ClickHouse {
    pub(crate) async fn session_summaries(
        &self,
        scope: HistoryScope,
        filter: &LogicalSessionListFilter,
        identities: Option<&[(uuid::Uuid, String)]>,
        directory: Vec<SessionDirectory>,
    ) -> Result<Vec<LogicalSessionSummary>, HistoryError> {
        if filter.legacy_cursor && filter.key_id.is_none() {
            return Err(HistoryError::Invalid(
                "legacy session cursor requires key_id",
            ));
        }
        let query = crate::request_history_api::query_from_request_filter(
            scope,
            &RequestListFilter {
                limit: 500,
                key_id: filter.key_id,
                ..Default::default()
            },
        )?;
        let mut sessions = BTreeMap::<(uuid::Uuid, String), Session>::new();
        self.scan(query, |snapshot| {
            // The existing session directory is native/archive, generation
            // usage has its own unlinked usage bucket rather than a session.
            if snapshot.identity.source == "generation" {
                return Ok(());
            }
            let view: RequestView =
                serde_json::from_value(snapshot.payload.ok_or(HistoryError::Mismatch)?)?;
            let id = view
                .session_context
                .as_ref()
                .and_then(|c| c.session_id.clone())
                .unwrap_or_else(|| format!("unlinked:{}", snapshot.identity.key_id));
            if identities
                .is_some_and(|values| !values.contains(&(snapshot.identity.key_id, id.clone())))
            {
                return Ok(());
            }
            let session = sessions.entry((snapshot.identity.key_id, id)).or_default();
            session.models.insert(view.model.clone());
            if let Some(name) = view
                .session_context
                .as_ref()
                .and_then(|c| c.session_name.as_ref())
            {
                session.names.insert(name.to_lowercase());
            }
            let active = matches!(
                view.lifecycle_state,
                RequestLifecycleState::Pending
                    | RequestLifecycleState::Preparing
                    | RequestLifecycleState::Queued
                    | RequestLifecycleState::Submitting
                    | RequestLifecycleState::Running
                    | RequestLifecycleState::Cancelling
            );
            let failed = matches!(
                view.lifecycle_state,
                RequestLifecycleState::Failed | RequestLifecycleState::Cancelled
            );
            if snapshot.identity.source == "archive" {
                session.archived += 1;
                session.archived_errors += i64::from(failed);
                session.archived_input += view.input_tokens;
                session.archived_output += view.output_tokens;
                if let Some(duration) = view.duration_ms {
                    session.archived_durations += 1;
                    session.archived_duration_sum += duration;
                }
            } else if active {
                session.active += 1;
            } else {
                session.count += 1;
                session.errors += i64::from(failed);
                session.input += view.input_tokens;
                session.output += view.output_tokens;
                if let Some(duration) = view.duration_ms {
                    session.durations += 1;
                    session.duration_sum += duration;
                }
                if let Some((cost, currency)) = snapshot
                    .statistics
                    .as_ref()
                    .and_then(|s| s.cost_micros.zip(s.currency.clone()))
                {
                    *session.costs.entry(currency).or_default() += cost;
                }
            }
            let order = (view.created_at, view.request_id);
            if session.latest_order.is_none_or(|previous| order > previous) {
                session.latest_order = Some(order);
                session.latest = Some(view);
            }
            Ok(())
        })
        .await?;
        for entry in directory {
            if filter.key_id.is_some_and(|key| key != entry.key_id)
                || identities
                    .is_some_and(|ids| !ids.contains(&(entry.key_id, entry.session_id.clone())))
            {
                continue;
            }
            if let Some(title) = &entry.title {
                sessions
                    .entry((entry.key_id, entry.session_id.clone()))
                    .or_default()
                    .names
                    .insert(title.to_lowercase());
            }
            let identity = (entry.key_id, entry.session_id.clone());
            sessions.entry(identity).or_default().directory = Some(entry);
        }
        let mut result = Vec::new();
        for ((key_id, session_id), session) in sessions {
            let latest = session.latest;
            let last_activity = latest
                .as_ref()
                .map(|v| v.created_at)
                .unwrap_or_default()
                .max(
                    session
                        .directory
                        .as_ref()
                        .map(|d| d.updated_at)
                        .unwrap_or_default(),
                );
            let unlinked = session_id.starts_with("unlinked:");
            let last_status = if session.active > 0 {
                "active"
            } else if latest
                .as_ref()
                .is_some_and(|v| matches!(v.lifecycle_state, RequestLifecycleState::Succeeded))
            {
                "success"
            } else if latest.as_ref().is_some_and(|v| {
                matches!(
                    v.lifecycle_state,
                    RequestLifecycleState::Failed | RequestLifecycleState::Cancelled
                )
            }) {
                "error"
            } else {
                "unknown"
            };
            if (filter.state == "active" && session.active == 0)
                || (filter.state == "has_errors" && session.errors + session.archived_errors == 0)
            {
                continue;
            }
            if filter
                .model
                .as_ref()
                .is_some_and(|m| !session.models.contains(m))
            {
                continue;
            }
            let context = latest.as_ref().and_then(|v| v.session_context.as_ref());
            let name = context.and_then(|c| c.session_name.clone());
            let key_alias = session
                .directory
                .as_ref()
                .map(|d| d.key_alias.clone())
                .or_else(|| {
                    latest
                        .as_ref()
                        .and_then(|v| v.credential_identity.as_ref())
                        .map(|c| c.key_alias.clone())
                })
                .unwrap_or_else(|| "__retired_credential__".into());
            if filter.query.as_ref().is_some_and(|q| {
                let q = q.trim().to_lowercase();
                !session_id.to_lowercase().starts_with(&q)
                    && !session.names.iter().any(|name| name.starts_with(&q))
                    && !key_alias.to_lowercase().starts_with(&q)
            }) {
                continue;
            }
            if let Some((at, id, key)) = &filter.cursor {
                let cursor_key = if filter.legacy_cursor {
                    filter
                        .key_id
                        .ok_or(HistoryError::Invalid(
                            "legacy session cursor requires key_id",
                        ))?
                        .to_string()
                } else {
                    key.clone()
                };
                if (last_activity, session_id.as_str(), key_id.to_string())
                    >= (*at, id.as_str(), cursor_key)
                {
                    continue;
                }
            }
            result.push(LogicalSessionSummary {
                cluster_id: if unlinked {
                    None
                } else {
                    Some(session_id.parse().map_err(|_| HistoryError::Mismatch)?)
                },
                session_id,
                session_name: name,
                task_kind: context.and_then(|c| c.task_kind.clone()),
                unlinked,
                key_id,
                key_alias,
                model: latest.as_ref().map(|v| v.model.clone()).unwrap_or_default(),
                upstream_model: latest.as_ref().and_then(|v| v.upstream_model.clone()),
                protocol: latest
                    .as_ref()
                    .map(|v| v.protocol.clone())
                    .unwrap_or_default(),
                last_status: last_status.into(),
                last_activity_at: last_activity,
                active_requests: session.active,
                requests: session.count,
                errors: session.errors,
                input_tokens: session.input,
                output_tokens: session.output,
                avg_duration_ms: (session.durations > 0)
                    .then(|| session.duration_sum as f64 / session.durations as f64),
                costs: session
                    .costs
                    .into_iter()
                    .map(|(currency, cost)| UsageAnalysisCost {
                        currency,
                        cost: micros_to_decimal_string(cost),
                    })
                    .collect(),
                archived_only_requests: session.archived,
                archived_only_errors: session.archived_errors,
                archived_only_input_tokens: session.archived_input,
                archived_only_output_tokens: session.archived_output,
                archived_only_avg_duration_ms: (session.archived_durations > 0).then(|| {
                    session.archived_duration_sum as f64 / session.archived_durations as f64
                }),
            });
        }
        result.sort_by(|a, b| {
            b.last_activity_at
                .cmp(&a.last_activity_at)
                .then_with(|| b.session_id.cmp(&a.session_id))
                .then_with(|| b.key_id.cmp(&a.key_id))
        });
        if identities.is_none() {
            result.truncate((filter.limit.clamp(1, 100) + 1) as usize);
        }
        Ok(result)
    }

    pub(crate) async fn conversation_requests(
        &self,
        scope: HistoryScope,
        session_id: &str,
        filter: &ConversationDetailFilter,
    ) -> Result<Vec<ConversationRequestView>, HistoryError> {
        let unlinked = session_id.starts_with("unlinked:");
        if let HistoryScope::Key { key_id, .. } = scope
            && unlinked
            && session_id != format!("unlinked:{key_id}")
        {
            return Err(HistoryError::Invalid("session key identity"));
        }
        let mut query = crate::request_history_api::query_from_request_filter(
            scope,
            &RequestListFilter {
                limit: filter.limit.clamp(1, 200),
                before_created_at: filter.before_created_at,
                before_id: filter.before_request_id,
                ..Default::default()
            },
        )?;
        // Session detail has its established 200-row page contract.
        query.limit = filter.limit.clamp(1, 200) as usize;
        query.predicates.push(HistoryPredicate {
            dimension: Dimension::Source,
            comparison: Comparison::NotEquals,
            value: PredicateValue::Text("generation".into()),
        });
        query.predicates.push(if unlinked {
            HistoryPredicate {
                dimension: Dimension::SessionAssociation,
                comparison: Comparison::Equals,
                value: PredicateValue::Text("unlinked".into()),
            }
        } else {
            HistoryPredicate {
                dimension: Dimension::SessionId,
                comparison: Comparison::Equals,
                value: PredicateValue::Text(session_id.into()),
            }
        });
        self.snapshots(&query)
            .await?
            .into_iter()
            .filter(|s| s.identity.source != "generation")
            .map(|snapshot| {
                let mut request: RequestView =
                    serde_json::from_value(snapshot.payload.ok_or(HistoryError::Mismatch)?)?;
                request.credential_identity = None;
                let provenance = &snapshot.read_model["archive"]["provenance"];
                let archived = snapshot.identity.source == "archive";
                Ok(ConversationRequestView {
                    currency: request.currency.clone(),
                    request,
                    source: if archived { "session_archive" } else { "live" }.into(),
                    provenance: if archived {
                        "archive_unlinked"
                    } else {
                        "native"
                    }
                    .into(),
                    unlinked: archived,
                    archive_source: provenance["source"].as_str().map(str::to_owned),
                    external_request_id: provenance["external_request_id"]
                        .as_str()
                        .map(str::to_owned),
                    execution: serde_json::from_value(snapshot.read_model["execution"].clone())?,
                    structure: serde_json::from_value(snapshot.read_model["structure"].clone())?,
                })
            })
            .collect()
    }
}
