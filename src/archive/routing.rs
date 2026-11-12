//! Backend-qualified immutable CAS locations. Bare keys always mean legacy.
use super::{ArchiveStore, path};
use crate::error::AppError;
use object_store::{ObjectStore, path::Path};
use std::sync::Arc;

pub(super) const STAGING_PREFIX: &str = "stores/staging/";
pub(super) const PREFIX: &str = "stores/training/";
#[derive(Clone)]
pub(super) struct TrainingStore {
    pub store: Arc<dyn ObjectStore>,
    pub identity: String,
}

pub(super) fn split(location: &str) -> Result<Option<(&str, &str)>, AppError> {
    let Some(rest) = location.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let (identity, key) = rest.split_once('/').ok_or_else(invalid)?;
    if identity.len() != 64
        || !identity
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    // This namespace is exclusively immutable tenant CAS. Typed staging has a
    // separate namespace; durable roots and disposable caches stay legacy.
    if !path::is_any_v1_cas_location(key) || !key.starts_with("tenants/") {
        return Err(invalid());
    }
    path::archive_path(key)?;
    Ok(Some((identity, key)))
}

/// Parse only a canonical typed staging segment, never a caller supplied bucket
/// or backend alias. The UUID/enums are the same identities persisted in SQL.
pub(super) fn split_staging(location: &str) -> Result<Option<(&str, &str)>, AppError> {
    let Some(rest) = location.strip_prefix(STAGING_PREFIX) else {
        return Ok(None);
    };
    let (identity, key) = rest.split_once('/').ok_or_else(invalid)?;
    let parts: Vec<_> = key.split('/').collect();
    if parts.len() < 5 || parts[0] != "staging" {
        return Err(invalid());
    }
    let kind = match parts[1] {
        "proxy" => "proxy_request",
        "synchronous" => "synchronous_request",
        "generation" => "generation_job",
        _ => return Err(invalid()),
    };
    let typed = crate::archive_staging::ArchiveStagingKey::from_database(
        kind, parts[2], parts[3], parts[4],
    )?
    .with_backend_identity(Some(identity))?;
    path::archive_path(key)?;
    if !crate::archive_staging::locator_matches_prefix(location, &typed.canonical_prefix()) {
        return Err(invalid());
    }
    Ok(Some((identity, key)))
}

fn invalid() -> AppError {
    AppError::storage("archive_backend_reference_invalid")
}

impl ArchiveStore {
    pub(super) fn resolve(
        &self,
        location: &str,
    ) -> Result<(&Arc<dyn ObjectStore>, Path), AppError> {
        match split(location)?.or(split_staging(location)?) {
            Some((identity, key)) => {
                let training = self
                    .training
                    .as_ref()
                    .ok_or_else(|| AppError::storage("archive_training_backend_missing"))?;
                if training.identity != identity {
                    return Err(AppError::storage(
                        "archive_training_backend_identity_mismatch",
                    ));
                }
                Ok((&training.store, path::archive_path(key)?))
            }
            None => Ok((&self.inner, path::archive_path(location)?)),
        }
    }
    pub(crate) fn select_staging_key(
        &self,
        key: crate::archive_staging::ArchiveStagingKey,
    ) -> Result<crate::archive_staging::ArchiveStagingKey, AppError> {
        if key.backend_identity.is_some() {
            self.resolve(&key.canonical_prefix())?;
            return Ok(key);
        }
        if !self.training_staging_writes {
            return Ok(key);
        }
        let training = self
            .training
            .as_ref()
            .ok_or_else(|| AppError::storage("archive_training_backend_missing"))?;
        key.with_backend_identity(Some(&training.identity))
    }

    pub(super) fn new_cas_locator(&self, key: String) -> Result<String, AppError> {
        if !self.training_cas_writes {
            return Ok(key);
        }
        let training = self
            .training
            .as_ref()
            .ok_or_else(|| AppError::storage("archive_training_backend_missing"))?;
        Ok(format!("{PREFIX}{}/{key}", training.identity))
    }
}
