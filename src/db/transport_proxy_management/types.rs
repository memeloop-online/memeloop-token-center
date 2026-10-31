use super::*;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Member {
    pub id: Uuid,
    pub label: String,
    pub proxy_url: String,
    #[serde(default = "initial_proxy_version")]
    pub version: i64,
}

fn initial_proxy_version() -> i64 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberInput {
    pub id: Option<Uuid>,
    pub label: String,
    pub proxy_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateGroup {
    pub tenant_external_id: String,
    pub name: String,
    #[serde(alias = "proxy_ids", deserialize_with = "deserialize_member_inputs")]
    pub members: Vec<MemberInput>,
    #[serde(default)]
    pub probe_url: Option<String>,
    #[serde(default)]
    pub probe_attempts: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateGroup {
    pub tenant_external_id: String,
    pub expected_version: i64,
    pub name: String,
    #[serde(alias = "proxy_ids", deserialize_with = "deserialize_member_inputs")]
    pub members: Vec<MemberInput>,
    pub replacement_member_id: Option<Uuid>,
    #[serde(default)]
    pub probe_url: Option<String>,
    #[serde(default)]
    pub probe_attempts: Option<u8>,
}

fn deserialize_member_inputs<'input, Deserializer>(
    deserializer: Deserializer,
) -> Result<Vec<MemberInput>, Deserializer::Error>
where
    Deserializer: serde::Deserializer<'input>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    let references = values.first().is_some_and(Value::is_string);
    values
        .into_iter()
        .map(|value| {
            if value.is_string() != references {
                return Err(serde::de::Error::custom(
                    "members and proxy_ids must not be mixed",
                ));
            }
            if references {
                let id = serde_json::from_value::<Uuid>(value).map_err(serde::de::Error::custom)?;
                Ok(MemberInput {
                    id: Some(id),
                    label: String::new(),
                    proxy_url: None,
                })
            } else {
                serde_json::from_value(value).map_err(serde::de::Error::custom)
            }
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteGroup {
    pub tenant_external_id: String,
    pub expected_version: i64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct BindingStamp {
    pub selection_version: i64,
    pub binding_version: i64,
    pub group_id: Option<Uuid>,
    pub group_version: Option<i64>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthGroupReference {
    pub group_id: Uuid,
    pub expected_group_version: i64,
    pub initial_member_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct OAuthGroupSnapshot {
    pub reference: OAuthGroupReference,
    pub tenant_external_id: String,
    pub proxy_url: String,
    pub account_binding: Option<(Uuid, BindingStamp)>,
    pub selected_member_id: Uuid,
    pub selected_proxy_url: String,
    pub account_revision: Option<(i64, i64)>,
    pub session_id: Option<Uuid>,
}

impl OAuthGroupSnapshot {
    pub(crate) fn validate_proxy(
        &self,
        proxy: Option<(&str, crate::network::OutboundScope)>,
    ) -> Result<(), AppError> {
        if proxy
            != Some((
                self.selected_proxy_url.as_str(),
                crate::network::OutboundScope::Private,
            ))
        {
            return Err(conflict("proxy_group_binding_conflict"));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindGroup {
    pub tenant_external_id: String,
    pub group_id: Uuid,
    pub expected_group_version: i64,
    pub initial_member_id: Uuid,
    pub expected_binding_version: i64,
    pub expected_credential_generation: i64,
    pub expected_updated_at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnbindGroup {
    pub tenant_external_id: String,
    pub expected_group_version: i64,
    pub expected_binding_version: i64,
    pub expected_credential_generation: i64,
    pub expected_updated_at: i64,
    pub single_proxy_member_id: Uuid,
}

pub(super) fn members(
    input: Vec<MemberInput>,
    previous: &[Member],
) -> Result<Vec<Member>, AppError> {
    if !(1..=4).contains(&input.len()) {
        return Err(invalid());
    }
    let mut output: Vec<Member> = Vec::new();
    for item in input {
        validate_name(&item.label)?;
        let old = item
            .id
            .map(|id| {
                previous
                    .iter()
                    .find(|member| member.id == id)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        let proxy_url = item
            .proxy_url
            .or_else(|| old.map(|member| member.proxy_url.clone()))
            .ok_or_else(invalid)?;
        crate::provider::validate_codex_proxy_url(&proxy_url).map_err(|_| invalid())?;
        let id = item.id.unwrap_or_else(Uuid::now_v7);
        if output
            .iter()
            .any(|member| member.id == id || member.proxy_url == proxy_url)
        {
            return Err(invalid());
        }
        output.push(Member {
            id,
            label: item.label,
            proxy_url,
            version: old.map(|member| member.version).unwrap_or(1),
        });
    }
    Ok(output)
}

pub(crate) fn probe_settings(
    url: Option<String>,
    attempts: Option<u8>,
) -> Result<(Option<String>, i64), AppError> {
    let attempts = i64::from(attempts.unwrap_or(2));
    if !(1..=8).contains(&attempts) {
        return Err(invalid());
    }
    let url = url.and_then(|value| {
        let trimmed = value.trim().to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    });
    if let Some(url) = &url {
        if url.len() > 200 {
            return Err(invalid());
        }
        let parsed = url::Url::parse(url).map_err(|_| invalid())?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
        {
            return Err(invalid());
        }
    }
    Ok((url, attempts))
}

pub(super) fn view(
    row: &AnyRow,
    tenant_external_id: &str,
    members: Vec<Member>,
) -> Result<Value, AppError> {
    let members = members.into_iter().map(|member| {
        let parsed = url::Url::parse(&member.proxy_url).map_err(|_| AppError::internal("types_view_1"))?;
        Ok(serde_json::json!({"id":member.id,"label":member.label,"scheme":parsed.scheme(),"host":parsed.host_str(),"port":parsed.port().unwrap_or(1080),"remote_dns":true,"has_auth":!parsed.username().is_empty() || parsed.password().is_some()}))
    }).collect::<Result<Vec<_>, AppError>>()?;
    Ok(serde_json::json!({
        "id":row.try_get::<String,_>("id")?, "tenant_external_id":tenant_external_id,
        "name":row.try_get::<String,_>("name")?, "version":row.try_get::<i64,_>("version")?,
        "members":members, "bound_account_count":row.try_get::<i64,_>("bound_account_count")?,
        "probe_url":row.try_get::<Option<String>,_>("probe_url")?,
        "probe_attempts":row.try_get::<i64,_>("probe_attempts")?,
        "last_probe_status":row.try_get::<Option<String>,_>("last_probe_status")?,
        "last_probe_checked_at":row.try_get::<Option<i64>,_>("last_probe_checked_at")?,
        "created_at":row.try_get::<i64,_>("created_at")?,"updated_at":row.try_get::<i64,_>("updated_at")?
    }))
}
