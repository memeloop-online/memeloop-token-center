use super::*;
use memeloop_token_center::db::{
    GroupRoutingStrategy, UpdateGroupInput, UpdateGroupRoutingStrategyInput,
};

#[tokio::test]
async fn group_strategy_revisions_are_tenant_scoped_and_legacy_updates_cannot_clobber_them() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("strategy.db").display()
    )))
    .await
    .unwrap();
    let pepper = state.config.key_pepper.as_bytes();
    state
        .db
        .create_key(key_input("strategy-a", "a"), pepper)
        .await
        .unwrap();
    state
        .db
        .create_key(key_input("strategy-b", "b"), pepper)
        .await
        .unwrap();
    for kind in [GroupKind::Provider, GroupKind::Route] {
        let group = state
            .db
            .create_group(
                kind,
                CreateGroupInput {
                    tenant_external_id: "strategy-a".into(),
                    name: "strategy group".into(),
                },
            )
            .await
            .unwrap();
        assert!(group.routing_strategy.is_none());
        assert_eq!((group.routing_priority, group.strategy_version), (0, 0));
        let resource = if kind == GroupKind::Provider {
            "provider-groups"
        } else {
            "route-groups"
        };
        let path = format!("/internal/v1/{resource}/{}/routing-strategy", group.id);
        let missing_plugin = api_json(
            &state,
            "PUT",
            path.clone(),
            json!({
                "tenant_external_id": "strategy-a", "expected_updated_at": group.updated_at,
                "expected_strategy_version": 0, "routing_priority": 0,
                "routing_strategy": {"plugin_id": "missing-group-routing-plugin", "config": {}}
            }),
        )
        .await;
        assert_eq!(missing_plugin, StatusCode::BAD_REQUEST);
        let foreign_api = api_json(
            &state,
            "PUT",
            path,
            json!({
                "tenant_external_id": "strategy-b", "expected_updated_at": group.updated_at,
                "expected_strategy_version": 0, "routing_priority": 0, "routing_strategy": null
            }),
        )
        .await;
        assert_eq!(foreign_api, StatusCode::NOT_FOUND);
        let input = UpdateGroupRoutingStrategyInput {
            tenant_external_id: "strategy-a".into(),
            expected_updated_at: group.updated_at,
            expected_strategy_version: 0,
            routing_priority: 20,
            routing_strategy: Some(GroupRoutingStrategy {
                plugin_id: "fixture".into(),
                config: json!({"mode":"balanced"}),
            }),
        };
        let mut foreign = input.clone();
        foreign.tenant_external_id = "strategy-b".into();
        assert!(matches!(
            state
                .db
                .update_group_routing_strategy(kind, group.id, foreign)
                .await,
            Err(AppError::NotFound)
        ));
        let updated = state
            .db
            .update_group_routing_strategy(kind, group.id, input.clone())
            .await
            .unwrap();
        assert_eq!(
            (updated.routing_priority, updated.strategy_version),
            (20, 1)
        );
        assert!(updated.updated_at > group.updated_at);
        assert!(matches!(
            state
                .db
                .update_group_routing_strategy(kind, group.id, input.clone())
                .await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            state
                .db
                .update_group(
                    kind,
                    group.id,
                    UpdateGroupInput {
                        tenant_external_id: "strategy-a".into(),
                        name: "stale".into(),
                        expected_updated_at: group.updated_at,
                    }
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            state
                .db
                .replace_group_members(
                    kind,
                    group.id,
                    ReplaceGroupMembersInput {
                        tenant_external_id: "strategy-a".into(),
                        member_ids: vec![],
                        expected_updated_at: group.updated_at,
                    }
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        let renamed = state
            .db
            .update_group(
                kind,
                group.id,
                UpdateGroupInput {
                    tenant_external_id: "strategy-a".into(),
                    name: "renamed".into(),
                    expected_updated_at: updated.updated_at,
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.strategy_version, 1);
        assert_eq!(
            renamed.routing_strategy.unwrap().config,
            json!({"mode":"balanced"})
        );
        let mut clear = input;
        clear.expected_updated_at = renamed.updated_at;
        clear.expected_strategy_version = 1;
        clear.routing_strategy = None;
        clear.routing_priority = -10;
        let cleared = state
            .db
            .update_group_routing_strategy(kind, group.id, clear)
            .await
            .unwrap();
        assert_eq!(
            (cleared.routing_priority, cleared.strategy_version),
            (-10, 2)
        );
        assert!(cleared.routing_strategy.is_none());
        assert!(
            state
                .db
                .list_groups(kind, "strategy-b")
                .await
                .unwrap()
                .is_empty()
        );
    }
    let credential = state
        .db
        .create_group(
            GroupKind::Credential,
            CreateGroupInput {
                tenant_external_id: "strategy-a".into(),
                name: "presentation only".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        state
            .db
            .update_group_routing_strategy(
                GroupKind::Credential,
                credential.id,
                UpdateGroupRoutingStrategyInput {
                    tenant_external_id: "strategy-a".into(),
                    expected_updated_at: credential.updated_at,
                    expected_strategy_version: 0,
                    routing_priority: 1,
                    routing_strategy: None,
                }
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
}
