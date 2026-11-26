//! Synthetic fixtures exercise the production selector; no alternate policy.
use super::*;
use bytes::Bytes;
use uuid::Uuid;

#[tokio::test]
async fn synthetic_scoped_response_route_preserves_binding_and_revert_reads() {
    let mut archive = ArchiveStore::training_memory_for_test();
    archive.training_cas_writes = false;
    archive.training_staging_writes = false;
    let tenant = Uuid::now_v7();
    let a_key = Uuid::now_v7();
    let a = trusted_scope(tenant, a_key);
    let b = trusted_scope(tenant, Uuid::now_v7());
    let identity = archive.training.as_ref().unwrap().identity.clone();
    let input = serde_json::json!({"version":1,"rules":[{"tenant":tenant,"key":a_key,"purpose":"response","backend_identity":identity}]}).to_string();
    let rules = routing::ArchiveWriteRules::parse(Some(&input), Some(&identity)).unwrap();
    let empty = routing::ArchiveWriteRules::parse(None, Some(&identity)).unwrap();
    let old_primary = archive
        .new_response_staging_key(Uuid::now_v7(), Some(a))
        .unwrap();
    assert!(old_primary.backend_identity.is_none());
    archive.write_rules = rules.clone();
    assert!(archive.resolve(&old_primary.canonical_prefix()).is_ok());
    assert!(old_primary.backend_identity.is_none());
    for (scope, qualified) in [(Some(a), true), (Some(b), false), (None, false)] {
        archive.write_rules = rules.clone();
        let selected = archive
            .new_response_staging_key(Uuid::now_v7(), scope)
            .unwrap();
        assert_eq!(selected.backend_identity.is_some(), qualified);
        let mut writer = archive
            .start_writer(&format!("{}/body", selected.canonical_prefix()))
            .await
            .unwrap();
        writer
            .write(Bytes::from_static(b"synthetic original"))
            .await
            .unwrap();
        let staged = writer.finish_staged().await.unwrap();
        let object = archive
            .promote_staged_text_to_cas(tenant, &staged)
            .await
            .unwrap();
        assert_eq!(
            object.object_locator.starts_with("stores/training/"),
            qualified
        );
        archive.write_rules = empty.clone();
        assert_eq!(
            archive.get(&staged.object_locator).await.unwrap(),
            "synthetic original"
        );
        assert_eq!(
            archive
                .get_tenant_content(tenant, &object, 1024)
                .await
                .unwrap(),
            "synthetic original"
        );
        let (_, physical) = archive.resolve(&staged.object_locator).unwrap();
        let opposite = if qualified {
            &archive.inner
        } else {
            &archive.training.as_ref().unwrap().store
        };
        assert!(opposite.head(&physical).await.is_err());
        assert!(
            archive
                .get_tenant_content(Uuid::now_v7(), &object, 1024)
                .await
                .is_err()
        );
    }
    for invalid in [
        input.replace("response", "request"),
        input.replace(&identity, &"b".repeat(64)),
        input.replace("\"version\":1", "\"version\":2"),
    ] {
        assert!(routing::ArchiveWriteRules::parse(Some(&invalid), Some(&identity)).is_err());
    }
}

fn trusted_scope(tenant: Uuid, key: Uuid) -> TrustedArchiveWriteScope {
    TrustedArchiveWriteScope::from(&crate::model::AuthenticatedKey {
        key_id: key,
        tenant_id: tenant,
        principal_id: Uuid::now_v7(),
        account_id: Uuid::now_v7(),
        alias: "synthetic".into(),
        currency: "USD".into(),
        credential_generation: 1,
        policy: Default::default(),
    })
}
