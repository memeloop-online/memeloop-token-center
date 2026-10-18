//! Installer preferences and immutable operation snapshots. Secrets are only
//! serialized into authenticated encrypted envelopes, never HTTP responses.
use super::transport_proxy_management::{OAuthGroupReference, OAuthGroupSnapshot};
use super::*;
use std::net::SocketAddr;

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum InstallerNetworkInput {
    #[default]
    Default,
    Direct,
    Group {
        tenant_external_id: String,
        group_id: Uuid,
        group_version: i64,
        member_id: Uuid,
    },
    Custom {
        url: String,
        username: Option<String>,
        password: Option<String>,
    },
}

impl InstallerNetworkInput {
    pub(crate) fn tenant(&self) -> Option<&str> {
        match self {
            Self::Group {
                tenant_external_id, ..
            } => Some(tenant_external_id),
            _ => None,
        }
    }

    pub(crate) fn view(&self) -> Value {
        match self {
            Self::Custom {
                url,
                username,
                password,
            } => serde_json::json!({
                "mode":"custom", "url":url, "username":username,
                "has_password":password.as_ref().is_some_and(|value| !value.is_empty()),
            }),
            other => serde_json::to_value(other).unwrap_or(Value::Null),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct FrozenInstallerNetwork {
    pub mode: String,
    pub proxy_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub proxy_host: Option<String>,
    pub proxy_addresses: Vec<SocketAddr>,
    pub group: Option<OAuthGroupSnapshot>,
}

fn aad(kind: &str, actor: &str, operation: &str) -> Vec<u8> {
    format!("mtc/plugin-installer-network/v1/{kind}/{actor}/{operation}").into_bytes()
}

fn changed_selection() -> AppError {
    AppError::Conflict("installation operation already has a different network selection".into())
}

impl Database {
    pub(crate) async fn installer_network_preference(
        &self,
        actor: &str,
        key: &[u8],
    ) -> Result<InstallerNetworkInput, AppError> {
        let row = sqlx::query(
            "SELECT preference_ciphertext FROM plugin_installer_network_preferences WHERE actor=$1",
        )
        .bind(actor)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            open_private_json(
                &row.try_get::<String, _>("preference_ciphertext")?,
                key,
                &aad("preference", actor, ""),
            )
        })
        .transpose()
        .map(|value| value.unwrap_or_default())
    }

    async fn complete_installer_network_input(
        &self,
        actor: &str,
        mut input: InstallerNetworkInput,
        key: &[u8],
    ) -> Result<InstallerNetworkInput, AppError> {
        if let InstallerNetworkInput::Custom {
            url,
            username,
            password,
        } = &mut input
        {
            crate::network::checked_installer_proxy_url(url)?;
            if username
                .as_ref()
                .is_some_and(|value| value.len() > 1024 || value.chars().any(char::is_control))
                || password
                    .as_ref()
                    .is_some_and(|value| value.len() > 4096 || value.chars().any(char::is_control))
            {
                return Err(AppError::BadRequest(
                    "installation proxy credentials are invalid".into(),
                ));
            }
            if password.is_none() {
                if let InstallerNetworkInput::Custom {
                    url: old_url,
                    username: old_username,
                    password: old_password,
                } = self.installer_network_preference(actor, key).await?
                {
                    if *url == old_url && *username == old_username {
                        *password = old_password;
                    }
                }
            }
        }
        Ok(input)
    }

    pub(crate) async fn put_installer_network_preference(
        &self,
        actor: &str,
        input: InstallerNetworkInput,
        key: &[u8],
    ) -> Result<Value, AppError> {
        let input = self
            .complete_installer_network_input(actor, input, key)
            .await?;
        self.resolve_installer_network(&input, key).await?;
        self.save_installer_network_preference(actor, &input, key)
            .await?;
        Ok(input.view())
    }

    async fn save_installer_network_preference(
        &self,
        actor: &str,
        input: &InstallerNetworkInput,
        key: &[u8],
    ) -> Result<(), AppError> {
        let ciphertext = seal_private_json(input, key, &aad("preference", actor, ""))?;
        sqlx::query("INSERT INTO plugin_installer_network_preferences(actor,preference_ciphertext,updated_at) VALUES ($1,$2,$3) ON CONFLICT(actor) DO UPDATE SET preference_ciphertext=excluded.preference_ciphertext,updated_at=excluded.updated_at")
            .bind(actor).bind(ciphertext).bind(unix_millis()).execute(&self.pool).await?;
        Ok(())
    }

    async fn resolve_installer_network(
        &self,
        input: &InstallerNetworkInput,
        key: &[u8],
    ) -> Result<FrozenInstallerNetwork, AppError> {
        let mut snapshot = FrozenInstallerNetwork {
            mode: match input {
                InstallerNetworkInput::Default => "direct",
                InstallerNetworkInput::Direct => "direct",
                InstallerNetworkInput::Group { .. } => "group",
                InstallerNetworkInput::Custom { .. } => "custom",
            }
            .into(),
            proxy_url: None,
            username: None,
            password: None,
            proxy_host: None,
            proxy_addresses: Vec::new(),
            group: None,
        };
        match input {
            InstallerNetworkInput::Default | InstallerNetworkInput::Direct => return Ok(snapshot),
            InstallerNetworkInput::Custom {
                url,
                username,
                password,
            } => {
                snapshot.proxy_url = Some(url.clone());
                snapshot.username = username.clone();
                snapshot.password = password.clone();
            }
            InstallerNetworkInput::Group {
                tenant_external_id,
                group_id,
                group_version,
                member_id,
            } => {
                let group = self
                    .snapshot_oauth_transport_group(
                        OAuthGroupReference {
                            group_id: *group_id,
                            expected_group_version: *group_version,
                            initial_member_id: *member_id,
                        },
                        tenant_external_id,
                        None,
                        key,
                    )
                    .await?;
                let mut parsed =
                    url::Url::parse(&group.selected_proxy_url).map_err(|_| AppError::Internal)?;
                let decode = |value: &str| {
                    percent_encoding::percent_decode_str(value)
                        .decode_utf8()
                        .map(|value| value.into_owned())
                        .map_err(|_| AppError::Internal)
                };
                snapshot.username = (!parsed.username().is_empty())
                    .then(|| decode(parsed.username()))
                    .transpose()?;
                snapshot.password = parsed.password().map(decode).transpose()?;
                parsed.set_username("").map_err(|_| AppError::Internal)?;
                parsed.set_password(None).map_err(|_| AppError::Internal)?;
                snapshot.proxy_url = Some(parsed.to_string());
                snapshot.group = Some(group);
            }
        }
        let (host, addresses) = crate::network::validate_installer_proxy_endpoint(
            snapshot.proxy_url.as_deref().ok_or(AppError::Internal)?,
            snapshot.group.is_some(),
        )
        .await?;
        snapshot.proxy_host = host;
        snapshot.proxy_addresses = addresses;
        Ok(snapshot)
    }

    /// The operation key is the opaque actor-bound installation idempotency
    /// hash, never a URL or credential. Omitted input replays the frozen choice.
    pub(crate) async fn freeze_installer_network(
        &self,
        operation_key: &str,
        actor: &str,
        input: Option<InstallerNetworkInput>,
        key: &[u8],
    ) -> Result<FrozenInstallerNetwork, AppError> {
        if operation_key.is_empty() || operation_key.len() > 256 {
            return Err(AppError::BadRequest(
                "invalid installation operation key".into(),
            ));
        }
        let existing = sqlx::query("SELECT actor,selection_digest,snapshot_ciphertext FROM plugin_installer_network_snapshots WHERE operation_key=$1")
            .bind(operation_key).fetch_optional(&self.pool).await?;
        if let Some(row) = existing {
            if row.try_get::<String, _>("actor")? != actor {
                return Err(AppError::Forbidden);
            }
            if let Some(input) = input {
                let input = self
                    .complete_installer_network_input(actor, input, key)
                    .await?;
                if selection_digest(&input, key)? != row.try_get::<String, _>("selection_digest")? {
                    return Err(changed_selection());
                }
            }
            return open_private_json(
                &row.try_get::<String, _>("snapshot_ciphertext")?,
                key,
                &aad("snapshot", actor, operation_key),
            );
        }
        let explicit = input.is_some();
        let input = match input {
            Some(input) => {
                self.complete_installer_network_input(actor, input, key)
                    .await?
            }
            None => self.installer_network_preference(actor, key).await?,
        };
        let snapshot = self.resolve_installer_network(&input, key).await?;
        let frozen = self
            .insert_installer_network_snapshot(
                operation_key,
                actor,
                &selection_digest(&input, key)?,
                &snapshot,
                key,
            )
            .await?;
        if explicit {
            self.save_installer_network_preference(actor, &input, key)
                .await?;
        }
        Ok(frozen)
    }

    async fn insert_installer_network_snapshot(
        &self,
        operation_key: &str,
        actor: &str,
        digest: &str,
        snapshot: &FrozenInstallerNetwork,
        key: &[u8],
    ) -> Result<FrozenInstallerNetwork, AppError> {
        let ciphertext = seal_private_json(snapshot, key, &aad("snapshot", actor, operation_key))?;
        sqlx::query("INSERT INTO plugin_installer_network_snapshots(operation_key,actor,selection_digest,snapshot_ciphertext,created_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(operation_key) DO NOTHING")
            .bind(operation_key).bind(actor).bind(digest).bind(ciphertext).bind(unix_millis()).execute(&self.pool).await?;
        let row = sqlx::query("SELECT actor,selection_digest,snapshot_ciphertext FROM plugin_installer_network_snapshots WHERE operation_key=$1")
            .bind(operation_key).fetch_one(&self.pool).await?;
        if row.try_get::<String, _>("actor")? != actor {
            return Err(AppError::Forbidden);
        }
        if row.try_get::<String, _>("selection_digest")? != digest {
            return Err(changed_selection());
        }
        open_private_json(
            &row.try_get::<String, _>("snapshot_ciphertext")?,
            key,
            &aad("snapshot", actor, operation_key),
        )
    }

    pub(crate) async fn load_installer_network(
        &self,
        operation_key: &str,
        actor: &str,
        key: &[u8],
    ) -> Result<FrozenInstallerNetwork, AppError> {
        let row = sqlx::query("SELECT snapshot_ciphertext FROM plugin_installer_network_snapshots WHERE operation_key=$1 AND actor=$2")
            .bind(operation_key).bind(actor).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        open_private_json(
            &row.try_get::<String, _>("snapshot_ciphertext")?,
            key,
            &aad("snapshot", actor, operation_key),
        )
    }

    pub(crate) async fn freeze_installer_network_from(
        &self,
        operation_key: &str,
        actor: &str,
        source_operation_key: &str,
        key: &[u8],
    ) -> Result<FrozenInstallerNetwork, AppError> {
        let row = sqlx::query("SELECT selection_digest,snapshot_ciphertext FROM plugin_installer_network_snapshots WHERE operation_key=$1 AND actor=$2")
            .bind(source_operation_key).bind(actor).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        let snapshot = open_private_json(
            &row.try_get::<String, _>("snapshot_ciphertext")?,
            key,
            &aad("snapshot", actor, source_operation_key),
        )?;
        self.insert_installer_network_snapshot(
            operation_key,
            actor,
            &row.try_get::<String, _>("selection_digest")?,
            &snapshot,
            key,
        )
        .await
    }
}

fn selection_digest(input: &InstallerNetworkInput, key: &[u8]) -> Result<String, AppError> {
    use hmac::{Hmac, Mac};
    // The digest stays server-side. Never add it or the input to audit/review.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| AppError::Internal)?;
    mac.update(b"mtc/plugin-installer-network/selection/v1");
    mac.update(&serde_json::to_vec(input).map_err(|_| AppError::Internal)?);
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests;
