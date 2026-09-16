use super::super::*;

#[derive(Debug, Deserialize, Default)]
pub(in crate::api) struct ModelsQuery {
    client_version: Option<String>,
}

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
    Query(query): Query<ModelsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let state = state.pin_application_plugins().await?;
    let sources = state
        .db
        .granted_model_capability_sources(key.key_id, key.tenant_id)
        .await?;
    if query
        .client_version
        .as_deref()
        .is_some_and(|version| !version.trim().is_empty())
        && crate::api::request_normalization::is_official_codex_user_agent(&headers)
    {
        return Ok(Json(codex_models_response(&state, &sources)));
    }
    let mut models = std::collections::BTreeMap::<String, ModelCapabilities>::new();
    for source in sources {
        let Some(provider) = state.providers.get(&source.driver) else {
            continue;
        };
        let capabilities = models.entry(source.public_model).or_default();
        capabilities.modalities.extend(
            downstream_modalities(
                &source.protocol,
                &source.driver,
                &source.upstream_model,
                &source.config_json,
                &provider.modalities,
            )
            .into_iter()
            .map(str::to_owned),
        );
        if source.protocol == "generation" {
            let schema = generation_parameter_schema(
                &source.driver,
                &source.upstream_model,
                &source.config_json,
            );
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

#[derive(Default)]
struct CodexModelAvailability {
    openai_source_seen: bool,
    openai_multi_agent_v2: bool,
    codex_model_metadata: Option<crate::provider::CodexModelMetadata>,
}

fn codex_models_response(
    state: &AppState,
    sources: &[crate::db::GrantedModelCapabilitySource],
) -> Value {
    let models = codex_model_availability(&state.providers, sources);
    json!({
        "models": models.into_iter().map(|(model, availability)| {
            codex_model_info(
                &model,
                availability.openai_source_seen && availability.openai_multi_agent_v2,
                availability.codex_model_metadata.as_ref(),
            )
        }).collect::<Vec<_>>()
    })
}

fn codex_model_availability(
    providers: &crate::provider::ProviderCatalog,
    sources: &[crate::db::GrantedModelCapabilitySource],
) -> std::collections::BTreeMap<String, CodexModelAvailability> {
    let mut models = std::collections::BTreeMap::<String, CodexModelAvailability>::new();
    for source in sources {
        if source.protocol != "openai" {
            continue;
        }
        if providers.get(&source.driver).is_none() {
            continue;
        }
        let availability = models.entry(source.public_model.clone()).or_default();
        let compatible = providers.supports_codex_multi_agent_v2_model_catalog(&source.driver);
        let metadata = if compatible {
            providers.codex_model_metadata_for_catalog(&source.driver)
        } else {
            None
        };
        if availability.openai_source_seen {
            // Later sources can only make the advertisement more
            // conservative. This avoids claiming V2 when a public model
            // can route to an incompatible OpenAI provider.
            availability.openai_multi_agent_v2 &= compatible;
            availability.codex_model_metadata = if availability.openai_multi_agent_v2 {
                match (availability.codex_model_metadata.take(), metadata) {
                    (Some(left), Some(right)) => Some(merge_codex_model_metadata(left, right)),
                    _ => None,
                }
            } else {
                None
            };
        } else {
            availability.openai_source_seen = true;
            availability.openai_multi_agent_v2 = compatible;
            availability.codex_model_metadata = metadata;
        }
    }
    models
}

fn merge_codex_model_metadata(
    left: crate::provider::CodexModelMetadata,
    right: crate::provider::CodexModelMetadata,
) -> crate::provider::CodexModelMetadata {
    let shell_type = if left.shell_type == right.shell_type {
        left.shell_type
    } else {
        "disabled".to_owned()
    };
    let apply_patch_tool_type = if left.apply_patch_tool_type == right.apply_patch_tool_type {
        left.apply_patch_tool_type
    } else {
        None
    };
    let context_window = if left.context_window == right.context_window {
        left.context_window
    } else {
        None
    };
    let input_modalities = left
        .input_modalities
        .into_iter()
        .filter(|modality| right.input_modalities.iter().any(|value| value == modality))
        .collect();
    crate::provider::CodexModelMetadata {
        shell_type,
        apply_patch_tool_type,
        context_window,
        input_modalities,
        supports_image_detail_original: left.supports_image_detail_original
            && right.supports_image_detail_original,
    }
}

fn codex_model_info(
    model: &str,
    multi_agent_v2: bool,
    metadata: Option<&crate::provider::CodexModelMetadata>,
) -> Value {
    let metadata = metadata.filter(|_| multi_agent_v2);
    let mut info = json!({
        "slug": model,
        "display_name": model,
        "description": null,
        "supported_reasoning_levels": [],
        "shell_type": metadata.map_or("disabled", |metadata| metadata.shell_type.as_str()),
        "visibility": "list",
        "supported_in_api": true,
        "priority": 0,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "availability_nux": null,
        "upgrade": null,
        "model_messages": null,
        "base_instructions": "",
        "include_skills_usage_instructions": false,
        "include_plugin_usage_instructions": false,
        "include_apps_usage_instructions": false,
        "supports_reasoning_summary_parameter": false,
        "default_reasoning_summary": "auto",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": metadata.and_then(|metadata| metadata.apply_patch_tool_type.as_deref()),
        "web_search_tool_type": "text",
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_image_detail_original": metadata
            .is_some_and(|metadata| metadata.supports_image_detail_original),
        "context_window": metadata.and_then(|metadata| metadata.context_window),
        "auto_compact_token_limit": null,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": metadata.map_or_else(
            || vec!["text".to_owned()],
            |metadata| metadata.input_modalities.clone(),
        ),
        "supports_search_tool": false,
        "supports_experimental_context": false,
        "use_responses_lite": false,
        "node_repl_auto_review_required": false,
        "node_repl_disabled": true,
        "multi_agent_version": "disabled",
    });
    if multi_agent_v2 {
        info["multi_agent_version"] = Value::String("v2".into());
    }
    info
}

fn downstream_modalities<'a>(
    protocol: &str,
    driver: &str,
    upstream_model: &str,
    config_json: &str,
    provider_modalities: &'a [String],
) -> Vec<&'a str> {
    let openai_compatible_http = crate::provider::is_openai_compatible_http_driver(driver);
    let siliconflow_video = driver == "http-json"
        && serde_json::from_str::<Value>(config_json)
            .ok()
            .is_some_and(|config| {
                crate::generation::is_siliconflow_video_profile(&config, upstream_model)
            });
    let builtin: &[&str] = match protocol {
        "generation" if siliconflow_video => &["image", "video"],
        "generation" if openai_compatible_http => &["image"],
        "generation" if driver == "volcengine-seedance" => &["video"],
        "generation" if driver == "comfyui" => &["image", "video"],
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
        "audio" => &["audio"],
        "generation" => &["image", "video"],
        _ => &[],
    };
    provider_modalities
        .iter()
        .map(String::as_str)
        .filter(|modality| allowed.contains(modality))
        .collect()
}

fn generation_parameter_schema(
    driver: &str,
    upstream_model: &str,
    config_json: &str,
) -> Option<Value> {
    let config: Value = serde_json::from_str(config_json).ok()?;
    match driver {
        "comfyui" => crate::generation::comfyui_parameter_schema(&config).ok(),
        "http-json" if crate::generation::is_siliconflow_video_profile(&config, upstream_model) => {
            Some(crate::generation::siliconflow_video_parameter_schema())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_model_info_is_conservative_and_marks_only_declared_multi_agent_models() {
        let providers = crate::provider::ProviderCatalog::builtins();
        let kimi_metadata = providers
            .get("kimi-oauth")
            .and_then(|provider| provider.request_compatibility.codex_model_metadata.as_ref())
            .expect("Kimi declares Codex model capabilities");
        let compatible = codex_model_info("kimi-k3-256k", true, Some(kimi_metadata));
        assert_eq!(compatible["slug"], "kimi-k3-256k");
        assert_eq!(compatible["multi_agent_version"], "v2");
        assert_eq!(compatible["shell_type"], "shell_command");
        assert_eq!(compatible["apply_patch_tool_type"], "freeform");
        assert_eq!(compatible["context_window"], 262144);
        assert_eq!(compatible["input_modalities"], json!(["text", "image"]));
        assert!(compatible.get("account_id").is_none());
        assert!(compatible.get("upstream_model").is_none());
        assert!(compatible.get("credential").is_none());

        let unknown = codex_model_info("unknown-compatible", true, None);
        assert_eq!(unknown["shell_type"], "disabled");
        assert_eq!(unknown["input_modalities"], json!(["text"]));

        let ordinary = codex_model_info("ordinary", false, Some(kimi_metadata));
        assert_eq!(ordinary["multi_agent_version"], "disabled");
        assert_eq!(ordinary["shell_type"], "disabled");
    }

    #[test]
    fn codex_catalog_treats_native_and_declared_kimi_routes_as_multi_agent_compatible() {
        fn source(model: &str, driver: &str) -> crate::db::GrantedModelCapabilitySource {
            crate::db::GrantedModelCapabilitySource {
                public_model: model.into(),
                upstream_model: "private-upstream-name".into(),
                protocol: "openai".into(),
                driver: driver.into(),
                config_json: "{}".into(),
            }
        }

        let providers = crate::provider::ProviderCatalog::builtins();
        let native = codex_model_availability(&providers, &[source("native-only", "openai-codex")]);
        assert!(native["native-only"].openai_multi_agent_v2);
        assert_eq!(
            native["native-only"]
                .codex_model_metadata
                .as_ref()
                .expect("native Codex metadata")
                .shell_type,
            "shell_command"
        );

        let kimi = codex_model_availability(&providers, &[source("kimi-only", "kimi-oauth")]);
        assert!(kimi["kimi-only"].openai_multi_agent_v2);
        let kimi_info = codex_model_info(
            "kimi-only",
            true,
            kimi["kimi-only"].codex_model_metadata.as_ref(),
        );
        assert_eq!(kimi_info["shell_type"], "shell_command");
        assert_eq!(kimi_info["apply_patch_tool_type"], "freeform");
        assert_eq!(kimi_info["context_window"], 262144);
        assert_eq!(kimi_info["input_modalities"], json!(["text", "image"]));

        let mixed = codex_model_availability(
            &providers,
            &[
                source("mixed", "openai-codex"),
                source("mixed", "kimi-oauth"),
            ],
        );
        assert!(mixed["mixed"].openai_multi_agent_v2);
        assert_eq!(
            mixed["mixed"]
                .codex_model_metadata
                .as_ref()
                .expect("compatible mixed metadata")
                .shell_type,
            "shell_command"
        );

        let incompatible = codex_model_availability(
            &providers,
            &[
                source("mixed-incompatible", "openai-codex"),
                source("mixed-incompatible", "http-json"),
            ],
        );
        assert!(!incompatible["mixed-incompatible"].openai_multi_agent_v2);
        assert!(
            incompatible["mixed-incompatible"]
                .codex_model_metadata
                .is_none()
        );
    }

    #[test]
    fn comfyui_parameter_schema_is_bounded_and_parameters_only() {
        let schema = generation_parameter_schema(
            "comfyui",
            "workflow",
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
        assert!(generation_parameter_schema("volcengine-seedance", "seedance", "{}").is_none());
        assert!(
            generation_parameter_schema(
                "comfyui",
                "workflow",
                r#"{"workflow_template":{"$mtc_param":"bad parameter"}}"#,
            )
            .is_none()
        );
        let siliconflow = generation_parameter_schema(
            "http-json",
            "Wan-AI/Wan2.2-T2V-A14B",
            r#"{"video_api":"siliconflow-v1","video_models":["Wan-AI/Wan2.2-T2V-A14B"]}"#,
        )
        .expect("fixed SiliconFlow parameter schema");
        assert_eq!(siliconflow["additionalProperties"], false);
        assert_eq!(
            siliconflow["properties"]["image_size"]["enum"],
            json!(["1280x720", "720x1280", "960x960"])
        );
        assert!(generation_parameter_schema("http-json", "image-model", "{}").is_none());
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
            downstream_modalities("generation", "http-json", "image-model", "{}", &advertised),
            vec!["image"]
        );
        assert_eq!(
            downstream_modalities(
                "generation",
                "http-json",
                "Wan-AI/Wan2.2-T2V-A14B",
                r#"{"video_api":"siliconflow-v1","video_models":["Wan-AI/Wan2.2-T2V-A14B"]}"#,
                &advertised,
            ),
            vec!["image", "video"]
        );
        assert_eq!(
            downstream_modalities(
                "generation",
                "volcengine-seedance",
                "seedance",
                "{}",
                &advertised
            ),
            vec!["video"]
        );
        assert_eq!(
            downstream_modalities("openai", "http-json", "text-model", "{}", &advertised),
            vec!["text", "embedding"]
        );
        assert_eq!(
            downstream_modalities(
                "generation",
                crate::provider::CBCNX_PROVIDER_DRIVER,
                "candidate-video-model",
                "{}",
                &advertised,
            ),
            vec!["image"],
            "CBCNX video candidates must not be advertised before a reviewed job adapter exists",
        );
    }
}
