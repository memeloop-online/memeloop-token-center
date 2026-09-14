//! The existing authenticated plugin feed also owns projection validation.
use std::sync::LazyLock;

use serde_json::Value;

use super::{PluginCapability, PluginManifest, PluginOperatorUiPresentation, PluginRuntime};
use crate::error::AppError;

static PROJECTION_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../schemas/plugin-ui-projection.schema.json"
    ))
    .expect("bundled projection schema is valid JSON")
});

impl PluginRuntime {
    pub(crate) fn validate_ui_projection(
        &self,
        plugin_id: &str,
        endpoint_id: &str,
        data: &Value,
    ) -> Result<(), AppError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .ok_or(AppError::NotFound)?;
        validate_projection(&plugin.manifest, endpoint_id, data)
    }
}

fn validate_projection(
    manifest: &PluginManifest,
    endpoint_id: &str,
    data: &Value,
) -> Result<(), AppError> {
    let slots: Vec<_> = manifest
        .contributions
        .operator_ui
        .iter()
        .filter(|slot| {
            slot.data_endpoint == endpoint_id
                && slot.presentation == Some(PluginOperatorUiPresentation::ProjectionV1)
        })
        .collect();
    if slots.is_empty() {
        return Ok(());
    }
    let invalid = || AppError::Upstream("plugin UI projection is invalid".into());
    crate::schema::validate_instance(&PROJECTION_SCHEMA, data).map_err(|_| invalid())?;
    if data["plugin_id"].as_str() != Some(manifest.id.as_str())
        || !slots
            .iter()
            .any(|slot| data["slot_id"].as_str() == Some(slot.id.as_str()))
    {
        return Err(invalid());
    }
    for component in data["components"].as_array().ok_or_else(invalid)? {
        if component["kind"].as_str() != Some("link") {
            continue;
        }
        let href = component["href"].as_str().ok_or_else(invalid)?;
        let url = url::Url::parse(href).map_err(|_| invalid())?;
        let origin = url.origin().ascii_serialization();
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || !manifest.capabilities.iter().any(|capability| {
                matches!(capability, PluginCapability::Http { allowed_origins } if allowed_origins.contains(&origin))
            })
        {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> PluginManifest {
        serde_json::from_value(json!({
            "id": "dashboard", "version": "1.0.0", "wit_version": "0.2.0",
            "capabilities": [{"kind": "http", "allowed_origins": ["https://example.com"]}],
            "contributions": {"operator_ui": [{
                "id": "summary", "slot": "operator.overview.card", "label": "Summary",
                "icon": "chart", "renderer": "typed_data_v1", "presentation": "projection_v1",
                "data_endpoint": "summary-data"
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn projection_feed_enforces_shape_identity_and_manifest_link_grants() {
        let manifest = manifest();
        let mut data = json!({"schema_version": 1, "plugin_id": "dashboard", "slot_id": "summary",
            "components": [{"kind": "link", "label": "Details", "href": "https://example.com/details"}]});
        assert!(validate_projection(&manifest, "summary-data", &data).is_ok());
        for href in [
            "https://other.example/details",
            "https://user@example.com",
            "javascript:alert(1)",
        ] {
            data["components"][0]["href"] = json!(href);
            assert!(validate_projection(&manifest, "summary-data", &data).is_err());
        }
        data["components"] = json!([]);
        data["plugin_id"] = json!("other-plugin");
        assert!(validate_projection(&manifest, "summary-data", &data).is_err());
        data["plugin_id"] = json!("dashboard");
        data["slot_id"] = json!("other-slot");
        assert!(validate_projection(&manifest, "summary-data", &data).is_err());
        data["slot_id"] = json!("summary");
        data["components"] = json!([{"kind": "html", "text": "<script>"}]);
        assert!(validate_projection(&manifest, "summary-data", &data).is_err());
        assert!(validate_projection(&manifest, "legacy-data", &json!({"legacy": true})).is_ok());
    }
}
