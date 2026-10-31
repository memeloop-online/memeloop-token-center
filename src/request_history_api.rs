use crate::{
    db::RequestListFilter,
    filter_ast::{TypedFilterOperator, TypedFilterValue},
    model::AuthenticatedKey,
    request_history::{
        Comparison, Dimension, HistoryCursor, HistoryError, HistoryPredicate, HistoryQuery,
        HistoryScope, PredicateValue,
    },
};

pub fn authenticated_key_scope(key: &AuthenticatedKey) -> HistoryScope {
    HistoryScope::Key {
        tenant_id: key.tenant_id,
        key_id: key.key_id,
    }
}

pub(crate) async fn list_if_enabled(
    state: &crate::AppState,
    scope: HistoryScope,
    filter: &RequestListFilter,
    paged: bool,
) -> Result<Option<serde_json::Value>, crate::error::AppError> {
    let Some(client) = state.db.history_reader().await? else {
        return Ok(None);
    };
    let query = query_from_request_filter(scope, filter)?;
    Ok(Some(client.list(&query).await?.into_api_response(paged)?))
}

pub(crate) async fn statistics_if_enabled(
    state: &crate::AppState,
    scope: HistoryScope,
    filter: &crate::db::StatsFilter,
) -> Result<Option<crate::model::OperatorStats>, crate::error::AppError> {
    let Some(client) = state.db.history_reader().await? else {
        return Ok(None);
    };
    let filter = RequestListFilter {
        from_created_at: filter.from_created_at,
        to_created_at: filter.to_created_at,
        key_id: filter.key_id,
        model: filter.model.clone(),
        protocol: filter.protocol.clone(),
        status: filter.status.clone(),
        error_code: filter.error_code.clone(),
        upstream_account_id: filter.upstream_account_id,
        route_id: filter.route_id,
        min_duration_ms: filter.min_duration_ms,
        max_duration_ms: filter.max_duration_ms,
        min_cost_micros: filter.min_cost_micros,
        max_cost_micros: filter.max_cost_micros,
        key_alias: filter.key_alias.clone(),
        principal: filter.principal.clone(),
        limit: 500,
        ..RequestListFilter::default()
    };
    let query = query_from_request_filter(scope, &filter)?;
    Ok(Some(client.statistics(&query).await?))
}

pub fn query_from_request_filter(
    authorized_scope: HistoryScope,
    filter: &RequestListFilter,
) -> Result<HistoryQuery, HistoryError> {
    if filter.before_created_at.is_some() != filter.before_id.is_some() {
        return Err(HistoryError::Invalid(
            "cursor fields must be supplied together",
        ));
    }
    let mut query = HistoryQuery {
        scope: authorized_scope.clone(),
        from_created_at: filter.from_created_at,
        to_created_at: filter.to_created_at,
        cursor: filter
            .before_created_at
            .zip(filter.before_id)
            .map(|(created_at, request_id)| HistoryCursor {
                before_created_at: created_at,
                before_id: request_id,
            }),
        limit: filter.limit.clamp(
            1,
            if matches!(authorized_scope, HistoryScope::Key { .. }) {
                100
            } else {
                500
            },
        ) as usize,
        predicates: Vec::new(),
    };
    for (dimension, value) in [
        (Dimension::KeyId, filter.key_id),
        (Dimension::UpstreamAccountId, filter.upstream_account_id),
        (Dimension::RouteId, filter.route_id),
    ] {
        if let Some(value) = value {
            if dimension == Dimension::KeyId && matches!(query.scope, HistoryScope::Key { .. }) {
                continue;
            }
            query.predicates.push(HistoryPredicate {
                dimension,
                comparison: Comparison::Equals,
                value: PredicateValue::Uuid(value),
            });
        }
    }
    for (dimension, value, comparison) in [
        (Dimension::Model, &filter.model, Comparison::Equals),
        (Dimension::Protocol, &filter.protocol, Comparison::Equals),
        (Dimension::Status, &filter.status, Comparison::Equals),
        (Dimension::ErrorCode, &filter.error_code, Comparison::Equals),
        (Dimension::KeyAlias, &filter.key_alias, Comparison::Prefix),
        (Dimension::Principal, &filter.principal, Comparison::Prefix),
    ] {
        if let Some(value) = value {
            query.predicates.push(HistoryPredicate {
                dimension,
                comparison,
                value: PredicateValue::Text(value.clone()),
            });
        }
    }
    for (dimension, min, max) in [
        (
            Dimension::DurationMs,
            filter.min_duration_ms,
            filter.max_duration_ms,
        ),
        (
            Dimension::CostMicros,
            filter.min_cost_micros,
            filter.max_cost_micros,
        ),
    ] {
        if min.zip(max).is_some_and(|(min, max)| min > max) {
            return Err(HistoryError::Invalid("numeric range"));
        }
        for (value, comparison) in [
            (min, Comparison::GreaterThanOrEqual),
            (max, Comparison::LessThanOrEqual),
        ] {
            if let Some(value) = value {
                query.predicates.push(HistoryPredicate {
                    dimension,
                    comparison,
                    value: PredicateValue::Integer(value),
                });
            }
        }
    }
    if let Some(ast) = &filter.typed_ast {
        ast.validate()
            .map_err(|_| HistoryError::Invalid("typed filter AST"))?;
        for condition in &ast.conditions {
            // Use the stable typed field wire name so the session owner's additive
            // enum can integrate without copying or editing their AST implementation.
            let field = serde_json::to_value(condition.field)?;
            let dimension = match field.as_str() {
                Some("created_at") => Dimension::CreatedAt,
                Some("key_id") => Dimension::KeyId,
                Some("model") => Dimension::Model,
                Some("protocol") => Dimension::Protocol,
                Some("status") => Dimension::Status,
                Some("error_code") => Dimension::ErrorCode,
                Some("upstream_account_id") => Dimension::UpstreamAccountId,
                Some("route_id") => Dimension::RouteId,
                Some("duration_ms") => Dimension::DurationMs,
                Some("cost_micros") => Dimension::CostMicros,
                Some("key_alias") => Dimension::KeyAlias,
                Some("principal") => Dimension::Principal,
                Some("session_association") => Dimension::SessionAssociation,
                _ => return Err(HistoryError::Invalid("unsupported typed history field")),
            };
            let comparison = match condition.operator {
                TypedFilterOperator::Equals => Comparison::Equals,
                TypedFilterOperator::NotEquals => Comparison::NotEquals,
                TypedFilterOperator::Contains => Comparison::Contains,
                TypedFilterOperator::GreaterThan => Comparison::GreaterThan,
                TypedFilterOperator::GreaterThanOrEqual | TypedFilterOperator::Between => {
                    Comparison::GreaterThanOrEqual
                }
                TypedFilterOperator::LessThan => Comparison::LessThan,
                TypedFilterOperator::LessThanOrEqual => Comparison::LessThanOrEqual,
            };
            let value = |value: &TypedFilterValue| match value {
                TypedFilterValue::Uuid(value) => PredicateValue::Uuid(*value),
                TypedFilterValue::Integer(value)
                | TypedFilterValue::Timestamp(value)
                | TypedFilterValue::MoneyMicros(value) => PredicateValue::Integer(*value),
                TypedFilterValue::Text(value)
                | TypedFilterValue::Model(value)
                | TypedFilterValue::Protocol(value)
                | TypedFilterValue::Status(value) => PredicateValue::Text(value.clone()),
            };
            query.predicates.push(HistoryPredicate {
                dimension,
                comparison,
                value: value(&condition.value),
            });
            if let Some(upper) = &condition.upper {
                query.predicates.push(HistoryPredicate {
                    dimension,
                    comparison: Comparison::LessThanOrEqual,
                    value: value(upper),
                });
            }
        }
    }
    query.validate()?;
    Ok(query)
}
