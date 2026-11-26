//! Isolated synthetic contract. No runtime configuration or production routing.
use super::*;
use crate::{
    archive_staging::{ArchiveStagingKey, ArchiveStagingOwner, ArchiveStagingPurpose},
    model::AuthenticatedKey,
};
use bytes::Bytes;
use serde::Deserialize;
use uuid::Uuid;

#[derive(Clone, Copy)]
struct TrustedScope {
    tenant: Uuid,
    key: Uuid,
}
impl From<&AuthenticatedKey> for TrustedScope {
    fn from(key: &AuthenticatedKey) -> Self {
        Self {
            tenant: key.tenant_id,
            key: key.key_id,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rules {
    version: u32,
    rules: Vec<Rule>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    tenant: Uuid,
    key: Uuid,
    purpose: String,
    backend_identity: String,
}
impl Rules {
    fn validate(&self, archive: &ArchiveStore) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported_version");
        }
        let mut seen = std::collections::HashSet::new();
        for rule in &self.rules {
            if rule.purpose != "response" {
                return Err("unsupported_purpose");
            }
            if archive.training.as_ref().map(|s| s.identity.as_str())
                != Some(rule.backend_identity.as_str())
            {
                return Err("unknown_backend");
            }
            if !seen.insert((rule.tenant, rule.key)) {
                return Err("duplicate_scope");
            }
        }
        Ok(())
    }
    fn select(
        &self,
        archive: &ArchiveStore,
        key: ArchiveStagingKey,
        scope: Option<TrustedScope>,
    ) -> Result<ArchiveStagingKey, crate::error::AppError> {
        // The sole existing selector remains responsible for existing bindings.
        if key.backend_identity.is_some() {
            return archive.select_staging_key(key);
        }
        if key.purpose == ArchiveStagingPurpose::Response {
            if let Some(scope) = scope {
                if let Some(rule) = self
                    .rules
                    .iter()
                    .find(|r| r.tenant == scope.tenant && r.key == scope.key)
                {
                    return archive.select_staging_key(
                        key.with_backend_identity(Some(&rule.backend_identity))?,
                    );
                }
            }
        }
        archive.select_staging_key(key)
    }
}

#[tokio::test]
async fn synthetic_scoped_response_route_preserves_binding_and_revert_reads() {
    let mut archive = ArchiveStore::training_memory_for_test();
    archive.training_cas_writes = false;
    archive.training_staging_writes = false;
    let tenant = Uuid::now_v7();
    let a = TrustedScope {
        tenant,
        key: Uuid::now_v7(),
    };
    let b = TrustedScope {
        tenant,
        key: Uuid::now_v7(),
    };
    let identity = archive.training.as_ref().unwrap().identity.clone();
    let rules: Rules = serde_json::from_value(serde_json::json!({"version":1,"rules":[{"tenant":tenant,"key":a.key,"purpose":"response","backend_identity":identity}]})).unwrap();
    rules.validate(&archive).unwrap();
    let empty = Rules {
        version: 1,
        ..Default::default()
    };
    empty.validate(&archive).unwrap();
    for scope in [Some(a), Some(b), None] {
        let key = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(Uuid::now_v7()),
            ArchiveStagingPurpose::Response,
            Uuid::now_v7(),
        )
        .unwrap();
        let selected = rules.select(&archive, key, scope).unwrap();
        let qualified = scope.is_some_and(|s| s.key == a.key);
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
        assert_eq!(empty.select(&archive, selected, Some(b)).unwrap(), selected);
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
}
