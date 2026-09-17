use super::super::*;
use std::collections::BTreeMap;

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
    {
        return Ok(Json(codex_models_response(&state.providers, &sources)));
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
    codex_model_capabilities: Option<crate::provider::CodexModelCapabilities>,
    third_party_capabilities_invalid: bool,
    native_source_seen: bool,
    third_party_source_seen: bool,
}

fn codex_models_response(
    providers: &crate::provider::ProviderCatalog,
    sources: &[crate::db::GrantedModelCapabilitySource],
) -> Value {
    let models = codex_model_availability(providers, sources);
    json!({
        "models": models.into_iter()
            // Codex replaces bundled entries by slug when applying this
            // response. Do not let a gateway-owned profile overwrite a
            // native Codex model whose complete bundled metadata we cannot
            // reproduce exactly here.
            .filter(|(model, availability)| should_advertise_codex_model(model, availability))
            .map(|(model, availability)| {
                codex_model_info(
                    &model,
                    availability.openai_source_seen && availability.openai_multi_agent_v2,
                    availability.codex_model_capabilities.as_ref(),
                )
            })
            .collect::<Vec<_>>()
    })
}

fn should_advertise_codex_model(model: &str, availability: &CodexModelAvailability) -> bool {
    availability.third_party_source_seen
        && !availability.native_source_seen
        && !availability.third_party_capabilities_invalid
        && availability.openai_multi_agent_v2
        && !crate::provider::is_bundled_codex_model_slug(model)
        && availability
            .codex_model_capabilities
            .as_ref()
            .is_some_and(usable_codex_model_capabilities)
}

fn usable_codex_model_capabilities(capabilities: &crate::provider::CodexModelCapabilities) -> bool {
    capabilities.version == crate::provider::CODEX_MODEL_CAPABILITIES_VERSION
        && capabilities.agent_instructions_template
            == crate::provider::CODEX_AGENT_INSTRUCTIONS_TEMPLATE_V1
        && capabilities.shell_type == "unified_exec"
        && capabilities.apply_patch_tool_type.as_deref() == Some("freeform")
        && capabilities
            .fallback_context_window
            .is_some_and(|window| window > 0)
        && capabilities
            .input_modalities
            .iter()
            .any(|modality| modality == "text")
        // The current Responses-via-Chat bridge maps only the supported image
        // detail vocabulary; it cannot preserve `original` semantics exactly.
        // Never advertise that capability until a versioned bridge implements
        // it without silent degradation.
        && !capabilities.supports_image_detail_original
        && if capabilities.supported_reasoning_levels.is_empty() {
            capabilities.default_reasoning_level.is_none()
        } else {
            capabilities
                .supported_reasoning_levels
                .iter()
                .all(|level| !level.effort.trim().is_empty() && !level.description.trim().is_empty())
                && capabilities
                    .default_reasoning_level
                    .as_ref()
                    .is_some_and(|default| {
                        capabilities
                            .supported_reasoning_levels
                            .iter()
                            .any(|level| &level.effort == default)
                    })
        }
        && capabilities.include_skills_usage_instructions
        && capabilities.include_plugin_usage_instructions
        && capabilities.include_apps_usage_instructions
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
        if source.driver == crate::oauth::codex_device::PROVIDER_DRIVER {
            availability.native_source_seen = true;
        } else {
            availability.third_party_source_seen = true;
        }
        let native = source.driver == crate::oauth::codex_device::PROVIDER_DRIVER;
        let compatible = providers.supports_codex_multi_agent_v2_model_catalog(&source.driver);
        let capabilities = if compatible && !native {
            providers.codex_model_capabilities_for_catalog(&source.driver)
        } else {
            None
        };
        if availability.openai_source_seen {
            // Later sources can only make the advertisement more
            // conservative. This avoids claiming V2 when a public model
            // can route to an incompatible OpenAI provider.
            availability.openai_multi_agent_v2 &= compatible;
        } else {
            availability.openai_source_seen = true;
            availability.openai_multi_agent_v2 = compatible;
        }
        if !native && compatible {
            let Some(capabilities) = capabilities else {
                // A compatible third-party candidate without its complete
                // versioned capability declaration must poison the alias.
                // Do not let a later source repopulate it based on ordering.
                availability.third_party_capabilities_invalid = true;
                availability.codex_model_capabilities = None;
                continue;
            };
            if availability.third_party_capabilities_invalid {
                continue;
            }
            let merged = match availability.codex_model_capabilities.take() {
                Some(left) => merge_codex_model_capabilities(left, capabilities),
                None => Some(capabilities),
            };
            if merged.is_none() {
                availability.third_party_capabilities_invalid = true;
            }
            availability.codex_model_capabilities = merged;
        }
    }
    models
}

fn merge_codex_model_capabilities(
    left: crate::provider::CodexModelCapabilities,
    right: crate::provider::CodexModelCapabilities,
) -> Option<crate::provider::CodexModelCapabilities> {
    if left.version != right.version {
        return None;
    }
    let agent_instructions_template =
        if left.agent_instructions_template == right.agent_instructions_template {
            left.agent_instructions_template
        } else {
            return None;
        };
    let shell_type = if left.shell_type == "unified_exec" && right.shell_type == "unified_exec" {
        "unified_exec".to_owned()
    } else {
        "disabled".to_owned()
    };
    let apply_patch_tool_type = if left.apply_patch_tool_type == right.apply_patch_tool_type {
        left.apply_patch_tool_type
    } else {
        None
    };
    let fallback_context_window =
        match (left.fallback_context_window, right.fallback_context_window) {
            (Some(left), Some(right)) => Some(left.min(right)),
            _ => None,
        };
    let input_modalities = left
        .input_modalities
        .into_iter()
        .filter(|modality| right.input_modalities.iter().any(|value| value == modality))
        .collect();
    let left_reasoning = reasoning_levels_by_effort(left.supported_reasoning_levels)?;
    let right_reasoning = reasoning_levels_by_effort(right.supported_reasoning_levels)?;
    let mut supported_reasoning_levels = Vec::new();
    for (effort, left_description) in left_reasoning {
        let Some(right_description) = right_reasoning.get(&effort) else {
            continue;
        };
        if left_description.as_str() != right_description.as_str()
            || left_description.trim().is_empty()
            || right_description.trim().is_empty()
        {
            return None;
        }
        supported_reasoning_levels.push(crate::provider::CodexReasoningLevel {
            effort,
            description: left_description,
        });
    }
    let default_reasoning_level = if left.default_reasoning_level == right.default_reasoning_level
        && left
            .default_reasoning_level
            .as_ref()
            .is_some_and(|default| {
                supported_reasoning_levels
                    .iter()
                    .any(|level| &level.effort == default)
            }) {
        left.default_reasoning_level
    } else {
        None
    };
    Some(crate::provider::CodexModelCapabilities {
        version: left.version,
        agent_instructions_template,
        shell_type,
        apply_patch_tool_type,
        fallback_context_window,
        input_modalities,
        supports_image_detail_original: left.supports_image_detail_original
            && right.supports_image_detail_original,
        include_skills_usage_instructions: left.include_skills_usage_instructions
            && right.include_skills_usage_instructions,
        include_plugin_usage_instructions: left.include_plugin_usage_instructions
            && right.include_plugin_usage_instructions,
        include_apps_usage_instructions: left.include_apps_usage_instructions
            && right.include_apps_usage_instructions,
        supported_reasoning_levels,
        default_reasoning_level,
    })
}

fn reasoning_levels_by_effort(
    levels: Vec<crate::provider::CodexReasoningLevel>,
) -> Option<BTreeMap<String, String>> {
    let mut by_effort = BTreeMap::new();
    for level in levels {
        if let Some(existing) = by_effort.get(&level.effort)
            && existing != &level.description
        {
            return None;
        }
        by_effort.insert(level.effort, level.description);
    }
    Some(by_effort)
}

fn codex_model_info(
    model: &str,
    multi_agent_v2: bool,
    capabilities: Option<&crate::provider::CodexModelCapabilities>,
) -> Value {
    let capabilities = capabilities
        .filter(|_| multi_agent_v2)
        .filter(|capabilities| usable_codex_model_capabilities(capabilities));
    let multi_agent_v2 = multi_agent_v2 && capabilities.is_some();
    let shell_type = capabilities.map_or("disabled", |capabilities| {
        match capabilities.shell_type.as_str() {
            "unified_exec" => "unified_exec",
            _ => "disabled",
        }
    });
    let instructions_template = capabilities
        .map(|capabilities| capabilities.agent_instructions_template.as_str())
        .map(|_| crate::provider::CODEX_GENERIC_AGENT_INSTRUCTIONS_V1);
    let model_messages = instructions_template.map(|template| {
        json!({
            "instructions_template": template,
        })
    });
    let mut info = json!({
        "slug": model,
        "display_name": model,
        "description": null,
        "shell_type": shell_type,
        "visibility": "list",
        "supported_in_api": true,
        "priority": 0,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "availability_nux": null,
        "upgrade": null,
        "model_messages": model_messages,
        "base_instructions": instructions_template,
        "include_skills_usage_instructions": capabilities
            .is_some_and(|capabilities| capabilities.include_skills_usage_instructions),
        "include_plugin_usage_instructions": capabilities
            .is_some_and(|capabilities| capabilities.include_plugin_usage_instructions),
        "include_apps_usage_instructions": capabilities
            .is_some_and(|capabilities| capabilities.include_apps_usage_instructions),
        "supports_reasoning_summary_parameter": false,
        "default_reasoning_summary": "auto",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": capabilities.and_then(|capabilities| capabilities.apply_patch_tool_type.as_deref()),
        // The bridge only preserves function/custom/namespace tools. Do not
        // advertise a built-in web-search tool that would be rejected or
        // silently dropped during Responses-to-Chat conversion.
        "web_search_tool_type": "disabled",
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_image_detail_original": capabilities
            .is_some_and(|capabilities| capabilities.supports_image_detail_original),
        "context_window": capabilities.and_then(|capabilities| capabilities.fallback_context_window),
        "max_context_window": capabilities
            .and_then(|capabilities| capabilities.fallback_context_window),
        "auto_compact_token_limit": null,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": capabilities.map_or_else(
            || vec!["text".to_owned()],
            |capabilities| capabilities.input_modalities.clone(),
        ),
        "supported_reasoning_levels": capabilities.map_or_else(Vec::new, |capabilities| {
            capabilities.supported_reasoning_levels.clone()
        }),
        "default_reasoning_level": capabilities
            .and_then(|capabilities| capabilities.default_reasoning_level.clone()),
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
        let kimi_capabilities = providers
            .get("kimi-oauth")
            .and_then(|provider| provider.codex_model_capabilities.as_ref())
            .expect("Kimi declares Codex model capabilities");
        let compatible = codex_model_info("kimi-k3-256k", true, Some(kimi_capabilities));
        assert_eq!(compatible["slug"], "kimi-k3-256k");
        assert_eq!(compatible["multi_agent_version"], "v2");
        assert_eq!(compatible["shell_type"], "unified_exec");
        assert_eq!(compatible["apply_patch_tool_type"], "freeform");
        assert_eq!(compatible["context_window"], 262144);
        assert_eq!(compatible["max_context_window"], 262144);
        assert_eq!(compatible["input_modalities"], json!(["text", "image"]));
        assert_eq!(compatible["default_reasoning_level"], "medium");
        assert_eq!(compatible["supported_reasoning_levels"][0]["effort"], "low");
        assert_eq!(
            compatible["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(compatible["supports_image_detail_original"], false);
        assert_eq!(compatible["web_search_tool_type"], "disabled");
        assert!(
            compatible["base_instructions"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert_eq!(
            compatible["model_messages"]["instructions_template"],
            compatible["base_instructions"]
        );
        assert_eq!(compatible["include_skills_usage_instructions"], true);
        assert_eq!(compatible["include_plugin_usage_instructions"], true);
        assert_eq!(compatible["include_apps_usage_instructions"], true);
        assert!(compatible.get("account_id").is_none());
        assert!(compatible.get("upstream_model").is_none());
        assert!(compatible.get("credential").is_none());

        let unknown = codex_model_info("unknown-compatible", true, None);
        assert_eq!(unknown["multi_agent_version"], "disabled");
        assert_eq!(unknown["shell_type"], "disabled");
        assert_eq!(unknown["input_modalities"], json!(["text"]));

        let mut unknown_profile = kimi_capabilities.clone();
        unknown_profile.agent_instructions_template = "future-profile".to_owned();
        let rejected_profile = codex_model_info("future-profile", true, Some(&unknown_profile));
        assert_eq!(rejected_profile["multi_agent_version"], "disabled");
        assert!(rejected_profile["base_instructions"].is_null());

        let mut mismatched_version = kimi_capabilities.clone();
        mismatched_version.version = "future-capabilities-v2".to_owned();
        assert!(
            merge_codex_model_capabilities(kimi_capabilities.clone(), mismatched_version).is_none()
        );
        let mut mismatched_description = kimi_capabilities.clone();
        mismatched_description.supported_reasoning_levels[0].description =
            "provider-specific wording".to_owned();
        assert!(
            merge_codex_model_capabilities(kimi_capabilities.clone(), mismatched_description)
                .is_none()
        );

        let mut reversed = kimi_capabilities.clone();
        reversed.supported_reasoning_levels.reverse();
        let merged_forward =
            merge_codex_model_capabilities(kimi_capabilities.clone(), reversed.clone())
                .expect("same reasoning levels merge");
        let merged_reverse = merge_codex_model_capabilities(reversed, kimi_capabilities.clone())
            .expect("same reasoning levels merge in reverse order");
        assert_eq!(
            serde_json::to_value(&merged_forward).unwrap(),
            serde_json::to_value(&merged_reverse).unwrap()
        );

        let mut subset = kimi_capabilities.clone();
        subset
            .supported_reasoning_levels
            .retain(|level| level.effort != "max");
        let merged_subset = merge_codex_model_capabilities(kimi_capabilities.clone(), subset)
            .expect("reasoning levels use the deterministic common intersection");
        assert_eq!(merged_subset.supported_reasoning_levels.len(), 4);

        let mut no_reasoning = kimi_capabilities.clone();
        no_reasoning.supported_reasoning_levels.clear();
        no_reasoning.default_reasoning_level = None;
        let strict_chat = codex_model_info("strict-chat", true, Some(&no_reasoning));
        assert_eq!(strict_chat["multi_agent_version"], "v2");
        assert_eq!(strict_chat["supported_reasoning_levels"], json!([]));
        assert!(strict_chat["default_reasoning_level"].is_null());

        let mut invalid_no_reasoning = no_reasoning;
        invalid_no_reasoning.default_reasoning_level = Some("medium".to_owned());
        let rejected = codex_model_info("invalid-strict-chat", true, Some(&invalid_no_reasoning));
        assert_eq!(rejected["multi_agent_version"], "disabled");

        let ordinary = codex_model_info("ordinary", false, Some(kimi_capabilities));
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
        assert!(native["native-only"].native_source_seen);
        assert!(!native["native-only"].third_party_source_seen);
        assert!(!should_advertise_codex_model(
            "native-only",
            &native["native-only"]
        ));
        assert_eq!(
            native["native-only"].codex_model_capabilities.as_ref(),
            None
        );
        assert_eq!(
            codex_models_response(&providers, &[source("native-only", "openai-codex")])["models"],
            json!([])
        );

        let kimi = codex_model_availability(&providers, &[source("kimi-only", "kimi-oauth")]);
        assert!(kimi["kimi-only"].openai_multi_agent_v2);
        assert!(should_advertise_codex_model(
            "kimi-only",
            &kimi["kimi-only"]
        ));

        let third_party_bundled_slug =
            codex_model_availability(&providers, &[source("gpt-5.5", "kimi-oauth")]);
        assert!(!should_advertise_codex_model(
            "gpt-5.5",
            &third_party_bundled_slug["gpt-5.5"]
        ));
        assert_eq!(
            codex_models_response(&providers, &[source("gpt-5.5", "kimi-oauth")])["models"],
            json!([])
        );
        let kimi_info = codex_model_info(
            "kimi-only",
            true,
            kimi["kimi-only"].codex_model_capabilities.as_ref(),
        );
        assert_eq!(kimi_info["shell_type"], "unified_exec");
        assert_eq!(kimi_info["apply_patch_tool_type"], "freeform");
        assert_eq!(kimi_info["context_window"], 262144);
        assert_eq!(kimi_info["input_modalities"], json!(["text", "image"]));
        assert_eq!(kimi_info["supports_image_detail_original"], false);
        assert_eq!(
            codex_models_response(&providers, &[source("kimi-only", "kimi-oauth")])["models"][0]["slug"],
            "kimi-only"
        );

        let mixed = codex_model_availability(
            &providers,
            &[
                source("mixed", "openai-codex"),
                source("mixed", "kimi-oauth"),
            ],
        );
        assert!(mixed["mixed"].openai_multi_agent_v2);
        assert!(mixed["mixed"].native_source_seen);
        assert!(mixed["mixed"].third_party_source_seen);
        assert!(!should_advertise_codex_model("mixed", &mixed["mixed"]));
        assert_eq!(
            mixed["mixed"]
                .codex_model_capabilities
                .as_ref()
                .expect("compatible mixed capabilities")
                .shell_type,
            "unified_exec"
        );
        assert_eq!(
            codex_models_response(
                &providers,
                &[
                    source("mixed", "openai-codex"),
                    source("mixed", "kimi-oauth"),
                ]
            )["models"],
            json!([])
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
                .codex_model_capabilities
                .is_none()
        );
    }

    #[test]
    fn codex_catalog_fails_closed_when_any_compatible_third_party_candidate_lacks_capabilities() {
        fn source(model: &str, driver: &str) -> crate::db::GrantedModelCapabilitySource {
            crate::db::GrantedModelCapabilitySource {
                public_model: model.into(),
                upstream_model: "private-upstream-name".into(),
                protocol: "openai".into(),
                driver: driver.into(),
                config_json: "{}".into(),
            }
        }

        let mut providers = crate::provider::ProviderCatalog::builtins();
        let mut incomplete = providers
            .get("kimi-oauth")
            .expect("builtin Kimi provider")
            .clone();
        incomplete.id = "kimi-incomplete-capabilities".into();
        incomplete.codex_model_capabilities = None;
        providers
            .extend([incomplete])
            .expect("cloned provider contribution is schema-valid");

        for sources in [
            vec![
                source("order-independent", "kimi-incomplete-capabilities"),
                source("order-independent", "kimi-oauth"),
            ],
            vec![
                source("order-independent", "kimi-oauth"),
                source("order-independent", "kimi-incomplete-capabilities"),
            ],
        ] {
            let availability = codex_model_availability(&providers, &sources);
            let availability = &availability["order-independent"];
            assert!(availability.openai_multi_agent_v2);
            assert!(availability.third_party_capabilities_invalid);
            assert!(availability.codex_model_capabilities.is_none());
            assert!(!should_advertise_codex_model(
                "order-independent",
                availability
            ));
            assert_eq!(
                codex_models_response(&providers, &sources)["models"],
                json!([])
            );
        }
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
