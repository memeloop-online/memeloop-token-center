use super::*;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Member {
    pub id: Uuid,
    pub label: String,
    pub proxy_url: String,
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
    pub members: Vec<MemberInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateGroup {
    pub tenant_external_id: String,
    pub expected_version: i64,
    pub name: String,
    pub members: Vec<MemberInput>,
    pub replacement_member_id: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteGroup {
    pub tenant_external_id: String,
    pub expected_version: i64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct BindingStamp {
    pub binding_version: i64,
    pub group_id: Option<Uuid>,
    pub group_version: Option<i64>,
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
        });
    }
    Ok(output)
}

pub(super) fn view(row: &AnyRow, tenant_external_id: &str, key: &[u8]) -> Result<Value, AppError> {
    let members = open_members(row, key)?.into_iter().map(|member| {
        let parsed = url::Url::parse(&member.proxy_url).map_err(|_| AppError::Internal)?;
        Ok(serde_json::json!({"id":member.id,"label":member.label,"scheme":"socks5h","remote_dns":true,"has_auth":!parsed.username().is_empty() || parsed.password().is_some()}))
    }).collect::<Result<Vec<_>, AppError>>()?;
    Ok(serde_json::json!({
        "id":row.try_get::<String,_>("id")?, "tenant_external_id":tenant_external_id,
        "name":row.try_get::<String,_>("name")?, "version":row.try_get::<i64,_>("version")?,
        "members":members, "bound_account_count":row.try_get::<i64,_>("bound_account_count")?,
        "created_at":row.try_get::<i64,_>("created_at")?,"updated_at":row.try_get::<i64,_>("updated_at")?
    }))
}
