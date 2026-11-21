//! Durable publication/reader coordination, not reference truth or delete permission.
//! Callers must pin every backend in the transitive reference closure before
//! publication starts. No transaction or connection is held during object I/O.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ArchiveGcScope {
    tenant: Uuid,
    backend: String,
}

impl ArchiveGcScope {
    pub(crate) fn new(tenant: Uuid, backend: &str) -> Result<Self, AppError> {
        if tenant.is_nil()
            || !(backend == "legacy"
                || (backend.len() == 64
                    && backend
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))))
        {
            return Err(AppError::storage("archive_gc_scope_invalid"));
        }
        Ok(Self {
            tenant,
            backend: backend.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveGcPinPurpose {
    Writer,
    Reader,
}
impl ArchiveGcPinPurpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::Writer => "writer",
            Self::Reader => "reader",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ArchiveGcPins {
    scopes: Vec<ArchiveGcScope>,
    token: Uuid,
    owner_kind: String,
    owner_id: Uuid,
    purpose: ArchiveGcPinPurpose,
}

#[derive(Clone, Debug)]
pub(crate) struct ArchiveGcExclusive {
    scope: ArchiveGcScope,
    token: Uuid,
    epoch: i64,
}

async fn lock_scope(tx: &mut Transaction<'_, Any>, scope: &ArchiveGcScope) -> Result<(), AppError> {
    if tx.as_mut().backend_name() == "PostgreSQL" {
        sqlx::query("SELECT root_epoch FROM archive_gc_scopes WHERE tenant_id=$1 AND backend_identity=$2 FOR UPDATE")
            .bind(scope.tenant.to_string()).bind(&scope.backend).fetch_optional(&mut **tx).await?;
    }
    Ok(())
}

impl Database {
    /// One atomic acquisition for all involved scopes, in fixed order. Tokens
    /// come from existing request/attempt/receipt ownership and survive retries.
    pub(crate) async fn acquire_archive_gc_pins(
        &self,
        mut scopes: Vec<ArchiveGcScope>,
        token: Uuid,
        owner_kind: &str,
        owner_id: Uuid,
        purpose: ArchiveGcPinPurpose,
    ) -> Result<Option<ArchiveGcPins>, AppError> {
        if token.is_nil() || owner_id.is_nil() || owner_kind.is_empty() || scopes.is_empty() {
            return Err(AppError::storage("archive_gc_owner_invalid"));
        }
        scopes.sort();
        scopes.dedup();
        let mut tx = self.begin_write_transaction().await?;
        for scope in &scopes {
            sqlx::query("INSERT INTO archive_gc_scopes (tenant_id,backend_identity) VALUES ($1,$2) ON CONFLICT(tenant_id,backend_identity) DO NOTHING")
                .bind(scope.tenant.to_string()).bind(&scope.backend).execute(&mut *tx).await?;
            lock_scope(&mut tx, scope).await?;
            let phase: String = sqlx::query_scalar(
                "SELECT phase FROM archive_gc_scopes WHERE tenant_id=$1 AND backend_identity=$2",
            )
            .bind(scope.tenant.to_string())
            .bind(&scope.backend)
            .fetch_one(&mut *tx)
            .await?;
            if phase != "open" {
                tx.rollback().await?;
                return Ok(None);
            }
            let old = sqlx::query("SELECT owner_kind,owner_id,purpose,active FROM archive_gc_pins WHERE tenant_id=$1 AND backend_identity=$2 AND token=$3")
                .bind(scope.tenant.to_string()).bind(&scope.backend).bind(token.to_string()).fetch_optional(&mut *tx).await?;
            if let Some(old) = old {
                if old.try_get::<i64, _>("active")? != 1 {
                    return Err(AppError::storage("archive_gc_pin_already_released"));
                }
                if old.try_get::<String, _>("owner_kind")? != owner_kind
                    || old.try_get::<String, _>("owner_id")? != owner_id.to_string()
                    || old.try_get::<String, _>("purpose")? != purpose.as_str()
                {
                    return Err(AppError::storage("archive_gc_pin_identity_conflict"));
                }
                continue;
            }
            let epoch: i64 = sqlx::query_scalar("UPDATE archive_gc_scopes SET root_epoch=root_epoch+1 WHERE tenant_id=$1 AND backend_identity=$2 AND phase='open' RETURNING root_epoch")
                .bind(scope.tenant.to_string()).bind(&scope.backend).fetch_one(&mut *tx).await?;
            sqlx::query("INSERT INTO archive_gc_pins (tenant_id,backend_identity,token,owner_kind,owner_id,purpose,acquired_epoch) VALUES ($1,$2,$3,$4,$5,$6,$7)")
                .bind(scope.tenant.to_string()).bind(&scope.backend).bind(token.to_string()).bind(owner_kind).bind(owner_id.to_string()).bind(purpose.as_str()).bind(epoch).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(Some(ArchiveGcPins {
            scopes,
            token,
            owner_kind: owner_kind.into(),
            owner_id,
            purpose,
        }))
    }

    /// Explicit owner completion only. Expired leases alone must not call this:
    /// the owning writer must first be fenced against late publication.
    pub(crate) async fn release_archive_gc_pins(
        &self,
        pins: &ArchiveGcPins,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        for scope in &pins.scopes {
            lock_scope(&mut tx, scope).await?;
            let released = sqlx::query("UPDATE archive_gc_pins SET active=0 WHERE tenant_id=$1 AND backend_identity=$2 AND token=$3 AND owner_kind=$4 AND owner_id=$5 AND purpose=$6 AND active=1")
                .bind(scope.tenant.to_string()).bind(&scope.backend).bind(pins.token.to_string()).bind(&pins.owner_kind).bind(pins.owner_id.to_string()).bind(pins.purpose.as_str()).execute(&mut *tx).await?.rows_affected() == 1;
            if released {
                sqlx::query("UPDATE archive_gc_scopes SET root_epoch=root_epoch+1 WHERE tenant_id=$1 AND backend_identity=$2")
                    .bind(scope.tenant.to_string()).bind(&scope.backend).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Capture before root enumeration. A missing scope has never been enrolled
    /// and cannot produce even a coordination snapshot for GC.
    pub(crate) async fn archive_gc_root_epoch(
        &self,
        scope: &ArchiveGcScope,
    ) -> Result<Option<i64>, AppError> {
        Ok(sqlx::query_scalar("SELECT root_epoch FROM archive_gc_scopes WHERE tenant_id=$1 AND backend_identity=$2 AND phase='open'")
            .bind(scope.tenant.to_string()).bind(&scope.backend).fetch_optional(&self.pool).await?)
    }

    /// This is only the coordination half of a proof. No object-delete method
    /// accepts this token until complete root enumeration and writer enrollment
    /// are implemented. Scan first, then atomically compare epoch/no pins.
    pub(crate) async fn claim_archive_gc_exclusive(
        &self,
        scope: &ArchiveGcScope,
        observed_epoch: i64,
        token: Uuid,
    ) -> Result<Option<ArchiveGcExclusive>, AppError> {
        if token.is_nil() {
            return Err(AppError::storage("archive_gc_owner_invalid"));
        }
        let mut tx = self.begin_write_transaction().await?;
        lock_scope(&mut tx, scope).await?;
        let changed = sqlx::query("UPDATE archive_gc_scopes SET phase='exclusive',gc_token=$1 WHERE tenant_id=$2 AND backend_identity=$3 AND phase='open' AND root_epoch=$4 AND NOT EXISTS(SELECT 1 FROM archive_gc_pins p WHERE p.tenant_id=archive_gc_scopes.tenant_id AND p.backend_identity=archive_gc_scopes.backend_identity AND p.active=1)")
            .bind(token.to_string()).bind(scope.tenant.to_string()).bind(&scope.backend).bind(observed_epoch).execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(changed.then(|| ArchiveGcExclusive {
            scope: scope.clone(),
            token,
            epoch: observed_epoch,
        }))
    }

    /// Ambiguous deletion remains fenced across process restart. There is no
    /// TTL or automatic transition back to open.
    pub(crate) async fn mark_archive_gc_delete_unknown(
        &self,
        exclusive: &ArchiveGcExclusive,
    ) -> Result<bool, AppError> {
        Ok(sqlx::query("UPDATE archive_gc_scopes SET phase='delete_unknown' WHERE tenant_id=$1 AND backend_identity=$2 AND root_epoch=$3 AND gc_token=$4 AND phase IN ('exclusive','delete_unknown')")
            .bind(exclusive.scope.tenant.to_string()).bind(&exclusive.scope.backend).bind(exclusive.epoch).bind(exclusive.token.to_string()).execute(&self.pool).await?.rows_affected()==1)
    }

    /// Only an unambiguous completed round can reopen. Unknown-result scopes
    /// require a separate exact-object reconciliation, not this transition.
    pub(crate) async fn finish_archive_gc_exclusive(
        &self,
        exclusive: &ArchiveGcExclusive,
    ) -> Result<bool, AppError> {
        Ok(sqlx::query("UPDATE archive_gc_scopes SET phase='open',gc_token=NULL,root_epoch=root_epoch+1 WHERE tenant_id=$1 AND backend_identity=$2 AND root_epoch=$3 AND gc_token=$4 AND phase='exclusive'")
            .bind(exclusive.scope.tenant.to_string()).bind(&exclusive.scope.backend).bind(exclusive.epoch).bind(exclusive.token.to_string()).execute(&self.pool).await?.rows_affected()==1)
    }
}

#[cfg(test)]
mod tests;
