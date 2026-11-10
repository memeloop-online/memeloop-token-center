//! Backend-qualified immutable CAS locations. Bare keys always mean legacy.
use super::{ArchiveStore, path};
use crate::error::AppError;
use object_store::{ObjectStore, path::Path};
use std::sync::Arc;

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
    // This first reader rollout qualifies immutable tenant CAS only. Staging,
    // reapers, durable roots and disposable caches retain legacy ownership.
    if !path::is_any_v1_cas_location(key) || !key.starts_with("tenants/") {
        return Err(invalid());
    }
    path::archive_path(key)?;
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
        match split(location)? {
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
