use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{HistoryCursor, HistoryError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    EmptyTenant,
    Key { tenant_id: Uuid, key_id: Uuid },
    Tenant(Uuid),
    GlobalOperator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dimension {
    CreatedAt,
    KeyId,
    Model,
    Protocol,
    Status,
    ErrorCode,
    UpstreamAccountId,
    RouteId,
    DurationMs,
    CostMicros,
    KeyAlias,
    Principal,
    SessionId,
    TaskKind,
    SessionAssociation,
    RequestId,
}

impl Dimension {
    fn column(self) -> &'static str {
        match self {
            Self::CreatedAt => "created_at",
            Self::KeyId => "key_id",
            Self::Model => "model",
            Self::Protocol => "protocol",
            Self::Status => "filter_status",
            Self::ErrorCode => "error_code",
            Self::UpstreamAccountId => "upstream_account_id",
            Self::RouteId => "route_id",
            Self::DurationMs => "duration_ms",
            Self::CostMicros => "filter_cost_micros",
            Self::KeyAlias => "key_alias",
            Self::Principal => "principal",
            Self::SessionId => "session_id",
            Self::TaskKind => "task_kind",
            Self::SessionAssociation => "session_id",
            Self::RequestId => "request_id",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Equals,
    NotEquals,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Contains,
    Prefix,
    IsNull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredicateValue {
    Text(String),
    Integer(i64),
    Uuid(Uuid),
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPredicate {
    pub dimension: Dimension,
    pub comparison: Comparison,
    pub value: PredicateValue,
}

#[derive(Clone, Debug)]
pub struct HistoryQuery {
    pub scope: HistoryScope,
    pub from_created_at: Option<i64>,
    pub to_created_at: Option<i64>,
    pub cursor: Option<HistoryCursor>,
    pub limit: usize,
    pub predicates: Vec<HistoryPredicate>,
}

#[derive(Clone, Debug)]
pub(crate) struct BoundQuery {
    pub sql: String,
    pub parameters: Vec<(String, String)>,
}

impl BoundQuery {
    pub fn new(sql: &str) -> Self {
        Self {
            sql: sql.to_owned(),
            parameters: Vec::new(),
        }
    }

    pub fn bind(&mut self, value: impl ToString, kind: &str) -> String {
        let name = format!("value_{}", self.parameters.len());
        self.parameters
            .push((format!("param_{name}"), value.to_string()));
        format!("{{{name}:{kind}}}")
    }
}

impl HistoryQuery {
    pub fn validate(&self) -> Result<(), HistoryError> {
        if self.limit == 0 {
            return Err(HistoryError::Invalid("empty page"));
        }
        let from = self.from_created_at.unwrap_or(0);
        let to = self.to_created_at.unwrap_or(i64::MAX);
        if from < 0 || to < from {
            return Err(HistoryError::Invalid("inclusive time window"));
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.before_created_at < 0 || cursor.before_id.is_nil())
        {
            return Err(HistoryError::Invalid("cursor"));
        }
        match self.scope {
            HistoryScope::Key { tenant_id, key_id } if tenant_id.is_nil() || key_id.is_nil() => {
                return Err(HistoryError::Invalid("key scope"));
            }
            HistoryScope::Tenant(tenant_id) if tenant_id.is_nil() => {
                return Err(HistoryError::Invalid("tenant scope"));
            }
            _ => {}
        }
        for predicate in &self.predicates {
            predicate.validate()?;
            if matches!(self.scope, HistoryScope::Key { .. })
                && matches!(
                    predicate.dimension,
                    Dimension::KeyAlias | Dimension::Principal
                )
            {
                return Err(HistoryError::Invalid("operator identity predicate"));
            }
        }
        Ok(())
    }

    pub(crate) fn latest(&self) -> Result<BoundQuery, HistoryError> {
        self.validate()?;
        let mut query =
            BoundQuery::new("SELECT * FROM (SELECT * FROM request_history_v1 FINAL WHERE ");
        match self.scope {
            HistoryScope::Key { tenant_id, key_id } => {
                let tenant = query.bind(tenant_id, "UUID");
                let key = query.bind(key_id, "UUID");
                query
                    .sql
                    .push_str(&format!("tenant_id = {tenant} AND key_id = {key}"));
            }
            HistoryScope::Tenant(tenant_id) => {
                let tenant = query.bind(tenant_id, "UUID");
                query.sql.push_str(&format!("tenant_id = {tenant}"));
            }
            HistoryScope::GlobalOperator => query.sql.push('1'),
            HistoryScope::EmptyTenant => query.sql.push('0'),
        }
        let from = query.bind(self.from_created_at.unwrap_or(0), "Int64");
        let to = query.bind(self.to_created_at.unwrap_or(i64::MAX), "Int64");
        query.sql.push_str(&format!(
            " AND created_at >= {from} AND created_at <= {to}) WHERE deleted = 0"
        ));
        for predicate in &self.predicates {
            predicate.append(&mut query)?;
        }
        Ok(query)
    }

    pub(crate) fn list_sql(&self) -> Result<BoundQuery, HistoryError> {
        let mut query = self.latest()?;
        if let Some(cursor) = &self.cursor {
            let created = query.bind(cursor.before_created_at, "Int64");
            let request = query.bind(cursor.before_id, "String");
            query.sql.push_str(&format!(
                " AND (created_at, toString(request_id)) < ({created}, {request})"
            ));
        }
        query.sql.push_str(" AND source != 'event'");
        let limit = query.bind(self.limit + 1, "UInt64");
        query.sql = format!(
            "SELECT snapshot, snapshot_hash FROM ({}) ORDER BY created_at DESC, toString(request_id) DESC LIMIT {limit} FORMAT JSONEachRow",
            query.sql
        );
        Ok(query)
    }

    pub(crate) fn daily_sql(&self) -> Result<BoundQuery, HistoryError> {
        let query = self.latest()?;
        Ok(BoundQuery {
            sql: format!(
                "SELECT intDiv(latest.created_at, 86400000) AS day_bucket, latest.outcome AS outcome, latest.currency AS currency, latest.billing_unit AS billing_unit, count() AS requests, sumOrNull(latest.input_tokens) AS input_tokens, count(latest.input_tokens) AS input_known_requests, sumOrNull(latest.output_tokens) AS output_tokens, count(latest.output_tokens) AS output_known_requests, sumOrNull(latest.cached_input_tokens) AS cached_input_tokens, sumOrNull(latest.cache_write_tokens) AS cache_write_tokens, sumOrNull(latest.generation_units) AS generation_units, sumOrNull(latest.cost_micros) AS cost_micros, count(latest.cost_micros) AS cost_known_requests FROM ({}) AS latest WHERE latest.source NOT IN ('archive', 'event') GROUP BY day_bucket, latest.outcome, latest.currency, latest.billing_unit ORDER BY day_bucket, latest.outcome, latest.currency, latest.billing_unit FORMAT JSONEachRow",
                query.sql
            ),
            parameters: query.parameters,
        })
    }
}

impl HistoryPredicate {
    fn validate(&self) -> Result<(), HistoryError> {
        let valid_type = match self.dimension {
            Dimension::CreatedAt | Dimension::DurationMs | Dimension::CostMicros => {
                matches!(self.value, PredicateValue::Integer(value) if value >= 0)
            }
            Dimension::KeyId
            | Dimension::RequestId
            | Dimension::UpstreamAccountId
            | Dimension::RouteId => {
                matches!(self.value, PredicateValue::Uuid(_))
                    || (self.comparison == Comparison::IsNull
                        && self.dimension != Dimension::KeyId
                        && self.value == PredicateValue::Null)
            }
            _ => {
                matches!(&self.value, PredicateValue::Text(value) if !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
            }
        };
        let valid_operator = match self.dimension {
            Dimension::CreatedAt | Dimension::DurationMs | Dimension::CostMicros => matches!(
                self.comparison,
                Comparison::Equals
                    | Comparison::NotEquals
                    | Comparison::GreaterThan
                    | Comparison::GreaterThanOrEqual
                    | Comparison::LessThan
                    | Comparison::LessThanOrEqual
            ),
            Dimension::KeyId
            | Dimension::RequestId
            | Dimension::SessionAssociation
            | Dimension::Protocol
            | Dimension::Status => {
                matches!(self.comparison, Comparison::Equals | Comparison::NotEquals)
            }
            Dimension::UpstreamAccountId | Dimension::RouteId => {
                matches!(
                    self.comparison,
                    Comparison::Equals | Comparison::NotEquals | Comparison::IsNull
                )
            }
            _ => matches!(
                self.comparison,
                Comparison::Equals
                    | Comparison::NotEquals
                    | Comparison::Contains
                    | Comparison::Prefix
            ),
        };
        if !valid_type || !valid_operator {
            return Err(HistoryError::Invalid("predicate type or operator"));
        }
        if self.dimension == Dimension::SessionAssociation
            && !matches!(&self.value, PredicateValue::Text(value) if matches!(value.as_str(), "confirmed" | "unlinked"))
        {
            return Err(HistoryError::Invalid("session association"));
        }
        if self.dimension == Dimension::Status
            && !matches!(&self.value, PredicateValue::Text(value) if matches!(value.as_str(), "success" | "error" | "pending" | "unknown"))
        {
            return Err(HistoryError::Invalid("observed status"));
        }
        Ok(())
    }

    fn append(&self, query: &mut BoundQuery) -> Result<(), HistoryError> {
        self.validate()?;
        let column = self.dimension.column();
        if self.dimension == Dimension::SessionAssociation {
            let unlinked = self.value == PredicateValue::Text("unlinked".into());
            let null = unlinked == (self.comparison == Comparison::Equals);
            query.sql.push_str(if null {
                " AND session_id IS NULL"
            } else {
                " AND session_id IS NOT NULL"
            });
            return Ok(());
        }
        if self.comparison == Comparison::IsNull {
            query.sql.push_str(&format!(" AND {column} IS NULL"));
            return Ok(());
        }
        let value = match &self.value {
            PredicateValue::Text(value) => {
                let value = if matches!(self.comparison, Comparison::Contains | Comparison::Prefix)
                {
                    value.trim()
                } else {
                    value.as_str()
                };
                query.bind(value, "String")
            }
            PredicateValue::Integer(value) => query.bind(value, "Int64"),
            PredicateValue::Uuid(value) => query.bind(value, "UUID"),
            PredicateValue::Null => return Err(HistoryError::Invalid("unexpected null predicate")),
        };
        let predicate = match self.comparison {
            Comparison::Equals => format!("{column} = {value}"),
            Comparison::NotEquals => format!("{column} != {value}"),
            Comparison::GreaterThan => format!("{column} > {value}"),
            Comparison::GreaterThanOrEqual => format!("{column} >= {value}"),
            Comparison::LessThan => format!("{column} < {value}"),
            Comparison::LessThanOrEqual => format!("{column} <= {value}"),
            Comparison::Contains => format!("positionCaseInsensitiveUTF8({column}, {value}) > 0"),
            Comparison::Prefix => format!("startsWith(lowerUTF8({column}), lowerUTF8({value}))"),
            Comparison::IsNull => unreachable!(),
        };
        query.sql.push_str(" AND (");
        query.sql.push_str(&predicate);
        query.sql.push(')');
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DailyBucket {
    pub day_bucket: i64,
    pub outcome: String,
    pub currency: Option<String>,
    pub billing_unit: Option<String>,
    pub requests: u64,
    pub input_tokens: Option<i64>,
    pub input_known_requests: u64,
    pub output_tokens: Option<i64>,
    pub output_known_requests: u64,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub generation_units: Option<i64>,
    pub cost_micros: Option<i64>,
    pub cost_known_requests: u64,
}
