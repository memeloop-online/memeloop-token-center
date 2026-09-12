//! Typed, allow-listed predicates used by the operator request filter.
//!
//! This module intentionally models *values* as tagged types rather than
//! accepting arbitrary JSON.  The SQL adapter only ever receives a member of
//! this closed set, so neither a field name nor an operator can become SQL.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;

pub const MAX_TYPED_FILTER_CONDITIONS: usize = 12;
pub const MAX_TYPED_FILTER_TEXT_BYTES: usize = 200;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypedFilterLogicalOperator {
    #[default]
    And,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypedFilterField {
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
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypedFilterOperator {
    Equals,
    NotEquals,
    Contains,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Between,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TypedFilterValue {
    Text(String),
    /// A model remains distinct from generic text in the wire contract.  The
    /// browser obtains it from the synchronized MTC model catalog.
    Model(String),
    Protocol(String),
    Status(String),
    Uuid(Uuid),
    Integer(i64),
    Timestamp(i64),
    MoneyMicros(i64),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TypedFilterCondition {
    pub field: TypedFilterField,
    pub operator: TypedFilterOperator,
    pub value: TypedFilterValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<TypedFilterValue>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TypedFilterAst {
    #[serde(default)]
    pub logical_operator: TypedFilterLogicalOperator,
    #[serde(default)]
    pub conditions: Vec<TypedFilterCondition>,
}

impl TypedFilterAst {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.conditions.len() > MAX_TYPED_FILTER_CONDITIONS {
            return Err(AppError::BadRequest(format!(
                "filter contains more than {MAX_TYPED_FILTER_CONDITIONS} conditions"
            )));
        }
        for condition in &self.conditions {
            validate_condition(condition)?;
        }
        Ok(())
    }

    pub fn uses_field(&self, field: TypedFilterField) -> bool {
        self.conditions
            .iter()
            .any(|condition| condition.field == field)
    }
}

fn validate_condition(condition: &TypedFilterCondition) -> Result<(), AppError> {
    let expected_value = expected_value_kind(condition.field);
    if !expected_value(&condition.value) {
        return Err(AppError::BadRequest(
            "filter value does not match its field type".into(),
        ));
    }
    match condition.operator {
        TypedFilterOperator::Between => {
            let Some(upper) = condition.upper.as_ref() else {
                return Err(AppError::BadRequest(
                    "between filters require an upper value".into(),
                ));
            };
            if !expected_value(upper) {
                return Err(AppError::BadRequest(
                    "filter upper value does not match its field type".into(),
                ));
            }
            if filter_integer(&condition.value) > filter_integer(upper) {
                return Err(AppError::BadRequest(
                    "filter range lower bound must not exceed upper bound".into(),
                ));
            }
        }
        _ if condition.upper.is_some() => {
            return Err(AppError::BadRequest(
                "only between filters accept an upper value".into(),
            ));
        }
        _ => {}
    }

    let allowed = match condition.field {
        TypedFilterField::CreatedAt
        | TypedFilterField::DurationMs
        | TypedFilterField::CostMicros => matches!(
            condition.operator,
            TypedFilterOperator::Equals
                | TypedFilterOperator::NotEquals
                | TypedFilterOperator::GreaterThan
                | TypedFilterOperator::GreaterThanOrEqual
                | TypedFilterOperator::LessThan
                | TypedFilterOperator::LessThanOrEqual
                | TypedFilterOperator::Between
        ),
        TypedFilterField::Model
        | TypedFilterField::ErrorCode
        | TypedFilterField::KeyAlias
        | TypedFilterField::Principal => matches!(
            condition.operator,
            TypedFilterOperator::Equals
                | TypedFilterOperator::NotEquals
                | TypedFilterOperator::Contains
        ),
        TypedFilterField::KeyId
        | TypedFilterField::UpstreamAccountId
        | TypedFilterField::RouteId
        | TypedFilterField::Protocol
        | TypedFilterField::Status => matches!(
            condition.operator,
            TypedFilterOperator::Equals | TypedFilterOperator::NotEquals
        ),
    };
    if !allowed {
        return Err(AppError::BadRequest(
            "filter operator is not allowed for this field".into(),
        ));
    }

    validate_value(&condition.value)?;
    if let Some(upper) = condition.upper.as_ref() {
        validate_value(upper)?;
    }
    // Protocol is historical request data, not the current route protocol
    // catalogue. The generic text guard below still bounds and rejects control
    // characters, while exact matching lets operators find imported protocol
    // names such as `openai-responses` without pretending they are routable.
    if let TypedFilterValue::Status(value) = &condition.value
        && !matches!(value.as_str(), "success" | "error" | "pending")
    {
        return Err(AppError::BadRequest(
            "filter status is not supported".into(),
        ));
    }
    Ok(())
}

fn expected_value_kind(field: TypedFilterField) -> fn(&TypedFilterValue) -> bool {
    match field {
        TypedFilterField::CreatedAt => |value| matches!(value, TypedFilterValue::Timestamp(_)),
        TypedFilterField::KeyId
        | TypedFilterField::UpstreamAccountId
        | TypedFilterField::RouteId => |value| matches!(value, TypedFilterValue::Uuid(_)),
        TypedFilterField::Model => |value| matches!(value, TypedFilterValue::Model(_)),
        TypedFilterField::Protocol => |value| matches!(value, TypedFilterValue::Protocol(_)),
        TypedFilterField::Status => |value| matches!(value, TypedFilterValue::Status(_)),
        TypedFilterField::ErrorCode | TypedFilterField::KeyAlias | TypedFilterField::Principal => {
            |value| matches!(value, TypedFilterValue::Text(_))
        }
        TypedFilterField::DurationMs => |value| matches!(value, TypedFilterValue::Integer(_)),
        TypedFilterField::CostMicros => |value| matches!(value, TypedFilterValue::MoneyMicros(_)),
    }
}

fn validate_value(value: &TypedFilterValue) -> Result<(), AppError> {
    match value {
        TypedFilterValue::Text(value)
        | TypedFilterValue::Model(value)
        | TypedFilterValue::Protocol(value)
        | TypedFilterValue::Status(value)
            if value.is_empty()
                || value.len() > MAX_TYPED_FILTER_TEXT_BYTES
                || value.chars().any(char::is_control) =>
        {
            return Err(AppError::BadRequest(format!(
                "filter text must contain 1 to {MAX_TYPED_FILTER_TEXT_BYTES} non-control characters"
            )));
        }
        TypedFilterValue::Timestamp(value) if *value < 0 => {
            return Err(AppError::BadRequest(
                "filter timestamps must be Unix milliseconds after the epoch".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn filter_text(value: &TypedFilterValue) -> &str {
    match value {
        TypedFilterValue::Text(value)
        | TypedFilterValue::Model(value)
        | TypedFilterValue::Protocol(value)
        | TypedFilterValue::Status(value) => value,
        _ => unreachable!("the AST is validated before SQL adaptation"),
    }
}

pub(crate) fn filter_uuid(value: &TypedFilterValue) -> Uuid {
    match value {
        TypedFilterValue::Uuid(value) => *value,
        _ => unreachable!("the AST is validated before SQL adaptation"),
    }
}

pub(crate) fn filter_integer(value: &TypedFilterValue) -> i64 {
    match value {
        TypedFilterValue::Integer(value)
        | TypedFilterValue::Timestamp(value)
        | TypedFilterValue::MoneyMicros(value) => *value,
        _ => unreachable!("the AST is validated before SQL adaptation"),
    }
}

/// Escape a user-visible substring for a portable SQL `LIKE` predicate.  The
/// query adapter supplies the fixed `ESCAPE '\\'` clause and still binds this
/// value rather than interpolating it into SQL.
pub(crate) fn search_contains(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('%');
    for character in value.trim().to_lowercase().chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('%');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_typed_operator_matrix() {
        let ast = TypedFilterAst {
            logical_operator: TypedFilterLogicalOperator::And,
            conditions: vec![TypedFilterCondition {
                field: TypedFilterField::CreatedAt,
                operator: TypedFilterOperator::Between,
                value: TypedFilterValue::Timestamp(1),
                upper: Some(TypedFilterValue::Timestamp(2)),
            }],
        };
        assert!(ast.validate().is_ok());

        let invalid = TypedFilterAst {
            conditions: vec![TypedFilterCondition {
                field: TypedFilterField::Model,
                operator: TypedFilterOperator::GreaterThan,
                value: TypedFilterValue::Model("model-a".into()),
                upper: None,
            }],
            ..ast
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn rejects_untyped_or_unbounded_complexity() {
        let condition = TypedFilterCondition {
            field: TypedFilterField::Model,
            operator: TypedFilterOperator::Equals,
            value: TypedFilterValue::Text("wrong-kind".into()),
            upper: None,
        };
        assert!(
            TypedFilterAst {
                conditions: vec![condition],
                ..TypedFilterAst::default()
            }
            .validate()
            .is_err()
        );
        assert_eq!(search_contains("a%_\\b"), "%a\\%\\_\\\\b%");
    }
}
