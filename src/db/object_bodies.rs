use super::*;
use crate::archive::{ArchiveStore, ConversationBodyReference};
use bytes::Bytes;

impl Database {
    pub(crate) async fn resolve_raw_archive_roots(
        &self,
        tenant_id: Uuid,
        mut refs: crate::model::RequestArchiveRefs,
    ) -> Result<crate::model::RequestArchiveRefs, AppError> {
        let archive = self.body_archive()?;
        let request_id = refs.view.request_id;
        if refs.view.status_code.is_some_and(|status| status >= 400)
            && let Some(summary) = archive.supplier_summary(tenant_id, request_id).await?
        {
            refs.view.supplier_error = Some(summary);
        }
        // Explicit metadata-only operations (for example audio audit records)
        // have no raw-root contract until their media capture is migrated.
        if refs.request_archive_state == crate::model::RequestArchiveState::MetadataOnly
            && refs.response_archive_state == crate::model::RequestArchiveState::MetadataOnly
        {
            return Ok(refs);
        }

        for side in ["request", "response"] {
            let (locator, state, reason) = if let Some(object) = archive
                .raw_capture_reference(tenant_id, request_id, side)
                .await?
            {
                (
                    object.object_locator,
                    crate::model::RequestArchiveState::Bound,
                    None,
                )
            } else {
                let failed = archive
                    .raw_capture_failed(tenant_id, request_id, side)
                    .await?;
                (
                    format!("gap://{request_id}/{side}"),
                    if failed {
                        crate::model::RequestArchiveState::Gap
                    } else {
                        crate::model::RequestArchiveState::Pending
                    },
                    failed.then(|| "capture_failed".to_owned()),
                )
            };
            if side == "request" {
                refs.request_object = locator;
                refs.request_archive_state = state;
                refs.request_archive_reason = reason;
            } else {
                refs.response_object = Some(locator);
                refs.response_json = None;
                refs.response_archive_state = state;
                refs.response_archive_reason = reason;
            }
        }
        refs.view.archive_state = if refs.request_archive_state
            == crate::model::RequestArchiveState::Gap
            || refs.response_archive_state == crate::model::RequestArchiveState::Gap
        {
            crate::model::RequestArchiveState::Gap
        } else if refs.request_archive_state == crate::model::RequestArchiveState::Pending
            || refs.response_archive_state == crate::model::RequestArchiveState::Pending
        {
            crate::model::RequestArchiveState::Pending
        } else {
            crate::model::RequestArchiveState::Bound
        };
        Ok(refs)
    }

    pub(crate) async fn bind_raw_capture(
        &self,
        tenant_id: Uuid,
        request_id: Uuid,
        side: &str,
        object: &crate::archive::CasArchiveObject,
    ) -> Result<bool, AppError> {
        self.body_archive()?
            .verify_tenant_content(tenant_id, object)
            .await?;
        let statement = match side {
            "request" => {
                "UPDATE request_records SET request_object = $1 WHERE id = $2 AND tenant_id = $3 AND (request_object IS NULL OR request_object = '' OR request_object = $1 OR request_object = 'gap://' || id || '/request' OR request_object = 'staging/proxy/' || id || '/request/' || id || '/body.json')"
            }
            "response" => {
                "UPDATE request_records SET response_object = $1 WHERE id = $2 AND tenant_id = $3 AND (response_object IS NULL OR response_object = '' OR response_object = $1 OR response_object = 'gap://' || id || '/response')"
            }
            _ => return Err(AppError::BadRequest("invalid raw archive side".into())),
        };
        let mut transaction = self.begin_write_transaction().await?;
        let changed = sqlx::query(statement)
            .bind(&object.object_locator)
            .bind(request_id.to_string())
            .bind(tenant_id.to_string())
            .execute(&mut *transaction)
            .await?;
        if changed.rows_affected() == 1 {
            super::requests::conversations::wake_session_body_archive_in_transaction(
                &mut transaction,
                request_id,
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(changed.rows_affected() == 1)
    }

    pub(crate) async fn conversation_object_context(
        &self,
        request_id: Uuid,
    ) -> Result<(Uuid, crate::conversation::ConversationHints, bool), AppError> {
        let row = sqlx::query(
            "SELECT o.tenant_id, o.hints_json, (SELECT CAST(CASE WHEN r.completed_at IS NOT NULL AND r.status_code BETWEEN 200 AND 299 AND r.error_code IS NULL THEN 1 ELSE 0 END AS BIGINT) FROM request_records r WHERE r.id = o.request_id LIMIT 1) AS complete FROM conversation_projection_outbox o WHERE o.request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let hints: String = row.try_get("hints_json")?;
        Ok((
            parse_uuid(row.try_get("tenant_id")?)?,
            serde_json::from_str(&hints)
                .map_err(|_| AppError::storage("conversation_hints_invalid"))?,
            row.try_get::<Option<i64>, _>("complete")? == Some(1),
        ))
    }

    pub fn with_archive(mut self, archive: ArchiveStore) -> Self {
        self.archive = Some(archive);
        self
    }

    pub(crate) fn body_archive(&self) -> Result<&ArchiveStore, AppError> {
        self.archive
            .as_ref()
            .ok_or_else(|| AppError::storage("conversation_archive_not_configured"))
    }

    pub(crate) async fn store_body_reference(
        &self,
        tenant_id: Uuid,
        document: String,
    ) -> Result<String, AppError> {
        let chars = document.chars().count() as u64;
        let object = self
            .body_archive()?
            .put_tenant_content(tenant_id, Bytes::from(document))
            .await?;
        serde_json::to_string(&ConversationBodyReference {
            conversation_body_v1: object,
            chars,
        })
        .map_err(|_| AppError::internal("conversation_body_reference_encode"))
    }

    pub(crate) async fn read_body_reference(
        &self,
        tenant_id: Uuid,
        encoded: &str,
    ) -> Result<String, AppError> {
        let reference: ConversationBodyReference = serde_json::from_str(encoded)
            .map_err(|_| AppError::storage("conversation_object_migration_required"))?;
        let (document, chars) = self
            .body_archive()?
            .read_tenant_text_range(
                tenant_id,
                &reference.conversation_body_v1,
                0..reference.chars,
            )
            .await?;
        if chars != reference.chars {
            return Err(AppError::storage(
                "conversation_body_character_count_mismatch",
            ));
        }
        Ok(document)
    }

    pub(crate) async fn read_body_reference_range(
        &self,
        tenant_id: Uuid,
        encoded: &str,
        offset: i64,
        limit: i64,
    ) -> Result<(String, i64), AppError> {
        let reference: ConversationBodyReference = serde_json::from_str(encoded)
            .map_err(|_| AppError::storage("conversation_object_migration_required"))?;
        let start = u64::try_from(offset)
            .map_err(|_| AppError::BadRequest("invalid text offset".into()))?;
        let end = start
            .checked_add(
                u64::try_from(limit)
                    .map_err(|_| AppError::BadRequest("invalid text limit".into()))?,
            )
            .ok_or_else(|| AppError::BadRequest("text range overflow".into()))?;
        let (text, chars) = self
            .body_archive()?
            .read_tenant_text_range(tenant_id, &reference.conversation_body_v1, start..end)
            .await?;
        if chars != reference.chars {
            return Err(AppError::storage(
                "conversation_body_character_count_mismatch",
            ));
        }
        Ok((
            text,
            i64::try_from(chars)
                .map_err(|_| AppError::storage("conversation_body_character_count_invalid"))?,
        ))
    }

    pub(crate) async fn archive_atom_bodies<
        Atom: std::borrow::Borrow<crate::conversation::SemanticAtom>,
    >(
        &self,
        tenant_id: Uuid,
        atoms: impl IntoIterator<Item = Atom>,
    ) -> Result<Vec<crate::conversation::SemanticAtom>, AppError> {
        let mut references = Vec::new();
        for owned_atom in atoms {
            let atom = owned_atom.borrow();
            let document = serde_json::to_string(&atom.content)
                .map_err(|_| AppError::internal("conversation_body_encode"))?;
            let reference = self.store_body_reference(tenant_id, document).await?;
            references.push(crate::conversation::SemanticAtom {
                role: atom.role.clone(),
                kind: atom.kind.clone(),
                content: serde_json::from_str(&reference)
                    .map_err(|_| AppError::internal("conversation_body_reference_decode"))?,
                content_hash: atom.content_hash.clone(),
                instance_hash: atom.instance_hash.clone(),
            });
        }
        Ok(references)
    }
}

pub(super) fn body_reference_chars(encoded: &str) -> Result<i64, AppError> {
    let reference: ConversationBodyReference = serde_json::from_str(encoded)
        .map_err(|_| AppError::storage("conversation_object_migration_required"))?;
    i64::try_from(reference.chars)
        .map_err(|_| AppError::storage("conversation_body_character_count_invalid"))
}

pub(super) fn text_slice(text: &str, offset: i64, limit: i64) -> Result<(String, i64), AppError> {
    let start = usize::try_from(offset)
        .map_err(|_| AppError::BadRequest("invalid conversation text offset".into()))?;
    let maximum = usize::try_from(limit)
        .map_err(|_| AppError::BadRequest("invalid conversation text limit".into()))?;
    let total = i64::try_from(text.chars().count())
        .map_err(|_| AppError::storage("conversation_text_length_invalid"))?;
    Ok((text.chars().skip(start).take(maximum).collect(), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn raw_capture_wakes_projection_before_financial_completion() {
        let state =
            crate::AppState::initialize(crate::config::Config::for_test("sqlite::memory:".into()))
                .await
                .unwrap();
        let pool = &state.db.pool;
        let identity = crate::db::ArchiveSpoolIdentity {
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            reservation_id: Uuid::now_v7(),
        };
        sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, response_object, reservation_id) VALUES ($1, $2, $3, 1, 'openai', 'test', 0, 0, 0, 'gap://' || $1 || '/request', 'gap://' || $1 || '/response', $4)")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(Uuid::now_v7().to_string()).bind(identity.reservation_id.to_string()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO conversation_projection_outbox (request_id, tenant_id, key_id, principal_id, observed_at, request_json, hints_json, lease_expires_at) VALUES ($1, $2, $3, $4, 1, '{}', '{}', 9223372036854775807)")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).execute(pool).await.unwrap();
        let object = state
            .archive
            .put_tenant_content(
                identity.tenant_id,
                Bytes::from_static(br#"{"original":"body"}"#),
            )
            .await
            .unwrap();
        for side in ["request", "response"] {
            state
                .archive
                .publish_raw_capture(identity.tenant_id, identity.request_id, side, &object)
                .await
                .unwrap();
            assert!(
                state
                    .db
                    .bind_raw_capture(identity.tenant_id, identity.request_id, side, &object)
                    .await
                    .unwrap()
            );
        }
        let row = sqlx::query("SELECT r.completed_at, r.response_object, o.lease_expires_at FROM request_records r JOIN conversation_projection_outbox o ON o.request_id = r.id WHERE r.id = $1").bind(identity.request_id.to_string()).fetch_one(pool).await.unwrap();
        assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
        assert_eq!(
            row.get::<String, _>("response_object"),
            object.object_locator
        );
        assert_eq!(row.get::<Option<i64>, _>("lease_expires_at"), None);
    }

    #[tokio::test]
    async fn reference_only_writer_reader_round_trip_and_fail_closed_legacy() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let tenant = Uuid::now_v7();
        let document = r#"{"role":"tool","content":"工具 result"}"#.to_owned();
        let encoded = database
            .store_body_reference(tenant, document.clone())
            .await
            .unwrap();
        assert!(!encoded.contains("result"));
        assert_eq!(
            database
                .read_body_reference(tenant, &encoded)
                .await
                .unwrap(),
            document
        );
        assert_eq!(
            body_reference_chars(&encoded).unwrap(),
            document.chars().count() as i64
        );
        assert!(
            database
                .read_body_reference(Uuid::now_v7(), &encoded)
                .await
                .is_err()
        );
        assert!(
            database
                .read_body_reference(tenant, &document)
                .await
                .is_err()
        );
        assert_eq!(text_slice("甲乙丙丁", 1, 2).unwrap(), ("乙丙".into(), 4));
    }
}
