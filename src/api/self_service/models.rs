use super::super::*;

#[derive(Default)]
struct ModelCapabilities {
    modalities: std::collections::BTreeSet<String>,
    generation_schema: Option<Value>,
    generation_schema_initialized: bool,
    generation_schema_conflicted: bool,
}

pub(in crate::api) async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let sources = state
        .db
        .granted_model_capability_sources(key.key_id, key.tenant_id)
        .await?;
    let mut models = std::collections::BTreeMap::<String, ModelCapabilities>::new();
    for source in sources {
        let Some(provider) = state.providers.get(&source.driver) else {
            continue;
        };
        let capabilities = models.entry(source.public_model).or_default();
        capabilities.modalities.extend(
            downstream_modalities(&source.protocol, &source.driver, &provider.modalities)
                .into_iter()
                .map(str::to_owned),
        );
        if source.protocol == "generation" {
            let schema = generation_parameter_schema(&source.driver, &source.config_json);
            if capabilities.generation_schema_initialized
                && capabilities.generation_schema != schema
            {
                capabilities.generation_schema_conflicted = true;
            } else if !capabilities.generation_schema_initialized {
                capabilities.generation_schema = schema;
                capabilities.generation_schema_initialized = true;
            }
        }
    }
    Ok(Json(json!({
        "object": "list",
        "data": models.into_iter().map(|(id, capabilities)| {
            let mut model = serde_json::Map::from_iter([
                ("id".to_owned(), Value::String(id)),
                ("object".to_owned(), Value::String("model".to_owned())),
                ("owned_by".to_owned(), Value::String("memeloop".to_owned())),
                ("modalities".to_owned(), json!(capabilities.modalities)),
            ]);
            if !capabilities.generation_schema_conflicted
                && let Some(schema) = capabilities.generation_schema
            {
                model.insert("generation_schema".to_owned(), schema);
            }
            Value::Object(model)
        }).collect::<Vec<_>>()
    })))
}

fn downstream_modalities<'a>(
    protocol: &str,
    driver: &str,
    provider_modalities: &'a [String],
) -> Vec<&'a str> {
    let builtin: &[&str] = match (protocol, driver) {
        ("generation", "http-json") => &["image"],
        ("generation", "volcengine-seedance") => &["video"],
        ("generation", "comfyui") => &["image", "video"],
        _ => &[],
    };
    if !builtin.is_empty() {
        return builtin
            .iter()
            .copied()
            .filter(|modality| provider_modalities.iter().any(|value| value == modality))
            .collect();
    }
    let allowed: &[&str] = match protocol {
        "openai" => &["text", "embedding"],
        "anthropic" => &["text"],
        "generation" => &["image", "video"],
        _ => &[],
    };
    provider_modalities
        .iter()
        .map(String::as_str)
        .filter(|modality| allowed.contains(modality))
        .collect()
}

fn generation_parameter_schema(driver: &str, config_json: &str) -> Option<Value> {
    if driver != "comfyui" {
        return None;
    }
    let config: Value = serde_json::from_str(config_json).ok()?;
    crate::generation::comfyui_parameter_schema(&config).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comfyui_parameter_schema_is_bounded_and_parameters_only() {
        let schema = generation_parameter_schema(
            "comfyui",
            &json!({
                "workflow_template": {
                    "1": {"inputs": {
                        "prompt": {"$mtc_param": "prompt"},
                        "seed": {"$mtc_param": "seed"}
                    }}
                },
                "parameter_schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["prompt", "seed"],
                    "properties": {
                        "prompt": {"title": "Prompt", "type": "string", "enum": ["cat", "dog"]},
                        "seed": {"type": "integer", "minimum": 0, "maximum": 100}
                    }
                }
            })
            .to_string(),
        )
        .expect("safe schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("prompt").is_some());
        assert_eq!(schema["properties"]["prompt"]["title"], "Prompt");
        assert_eq!(schema["properties"]["prompt"]["enum"][0], "cat");
        assert_eq!(schema["properties"]["seed"]["maximum"], 100);
        assert!(schema.get("input").is_none());
    }

    #[test]
    fn unsafe_or_non_comfyui_schema_fails_closed() {
        assert!(generation_parameter_schema("volcengine-seedance", "{}").is_none());
        assert!(
            generation_parameter_schema(
                "comfyui",
                r#"{"workflow_template":{"$mtc_param":"bad parameter"}}"#,
            )
            .is_none()
        );
    }

    #[test]
    fn builtin_protocols_do_not_overstate_modalities() {
        let advertised = vec![
            "text".to_owned(),
            "embedding".to_owned(),
            "image".to_owned(),
            "video".to_owned(),
        ];
        assert_eq!(
            downstream_modalities("generation", "http-json", &advertised),
            vec!["image"]
        );
        assert_eq!(
            downstream_modalities("generation", "volcengine-seedance", &advertised),
            vec!["video"]
        );
        assert_eq!(
            downstream_modalities("openai", "http-json", &advertised),
            vec!["text", "embedding"]
        );
    }
}
