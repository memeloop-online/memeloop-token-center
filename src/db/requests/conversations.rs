use std::collections::HashSet;

use super::super::*;

// Candidate selection reads at most 50 observations. Persisting the complete
// atom list for a near-limit request would otherwise let one ordinary request
// make the next request materialize hundreds of MiB of JSON. The full Merkle
// leaf remains in `leaf_node_hash`, so retries and true prefix continuations do
// not lose precision when the diagnostic fingerprint is capped.
const MAX_CONVERSATION_FINGERPRINT_ATOMS: usize = 1_024;
const MAX_CONVERSATION_FINGERPRINT_JSON_BYTES: usize = 70_000;

#[derive(Clone, Debug)]
pub struct ConversationListFilter {
    pub limit: i64,
    pub before_updated_at: Option<i64>,
    pub before_cluster_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct ConversationDetailFilter {
    pub limit: i64,
    pub before_created_at: Option<i64>,
    pub before_request_id: Option<Uuid>,
}

struct ConversationSelection {
    observation_id: String,
    cluster_id: String,
    relation: RelationKind,
    confidence: i64,
    direct_parent: bool,
    same_turn: bool,
    semantic_prefix: bool,
    compaction_overlap: bool,
    client_match: bool,
    write_edge: bool,
}

pub(crate) struct ConversationObservationInput<'a> {
    pub(crate) key: &'a AuthenticatedKey,
    pub(crate) request_id: Uuid,
    pub(crate) request_json: &'a serde_json::Value,
    pub(crate) hints: &'a ConversationHints,
    pub(crate) client_name: Option<&'a str>,
    pub(crate) observed_at: i64,
    /// Archive-only observations intentionally have no request_records row.
    pub(crate) attach_request_record: bool,
}

impl Database {
    pub async fn record_conversation_observation(
        &self,
        key: &AuthenticatedKey,
        request_id: Uuid,
        request_json: &serde_json::Value,
        hints: &ConversationHints,
        client_name: Option<&str>,
    ) -> Result<Uuid, AppError> {
        let mut transaction = self.pool.begin().await?;
        let cluster_id = self
            .record_conversation_observation_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key,
                    request_id,
                    request_json,
                    hints,
                    client_name,
                    observed_at: unix_millis(),
                    attach_request_record: true,
                },
            )
            .await?;
        transaction.commit().await?;
        Ok(cluster_id)
    }

    pub(crate) async fn record_conversation_observation_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Any>,
        input: ConversationObservationInput<'_>,
    ) -> Result<Uuid, AppError> {
        let ConversationObservationInput {
            key,
            request_id,
            request_json,
            hints,
            client_name,
            observed_at,
            attach_request_record,
        } = input;
        let atoms = extract_atoms(request_json);
        let nodes = build_prefix(&atoms);
        let atom_hashes = bounded_atom_hashes(&atoms);
        let atom_hashes_json =
            serde_json::to_string(&atom_hashes).map_err(|_| AppError::Internal)?;
        let leaf = nodes.last().map(|node| node.node_hash.clone());
        let now = observed_at;
        let observation_id = Uuid::now_v7();
        let request_id = request_id.to_string();
        let request_created_at = if attach_request_record {
            let locator = sqlx::query(
                "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
            )
            .bind(&request_id)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(AppError::NotFound)?;
            if locator.try_get::<String, _>("tenant_id")? != key.tenant_id.to_string()
                || locator.try_get::<String, _>("key_id")? != key.key_id.to_string()
            {
                return Err(AppError::NotFound);
            }
            Some(locator.try_get::<i64, _>("created_at")?)
        } else {
            None
        };

        for atom in &atoms {
            sqlx::query(
                "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(tenant_id, content_hash) DO NOTHING",
            )
            .bind(key.tenant_id.to_string())
            .bind(&atom.content_hash)
            .bind(&atom.instance_hash)
            .bind(&atom.role)
            .bind(&atom.kind)
            .bind(serde_json::to_string(&atom.content).map_err(|_| AppError::Internal)?)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
        }
        for node in &nodes {
            sqlx::query(
                "INSERT INTO context_nodes (tenant_id, node_hash, parent_hash, atom_hash, depth, created_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(tenant_id, node_hash) DO NOTHING",
            )
            .bind(key.tenant_id.to_string())
            .bind(&node.node_hash)
            .bind(&node.parent_hash)
            .bind(&node.atom_hash)
            .bind(node.depth as i64)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
        }

        let tenant_id = key.tenant_id.to_string();
        let principal_id = key.principal_id.to_string();
        let mut candidates = if hints.parent_turn_id.is_some()
            || hints.turn_id.is_some()
            || hints.session_id.is_some()
        {
            sqlx::query(
                "SELECT o.id, o.cluster_id, CASE WHEN LENGTH(o.atom_hashes_json) <= 70000 THEN o.atom_hashes_json ELSE '[]' END AS atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.created_at <= $7 AND (($4 IS NOT NULL AND (o.turn_id = $4 OR o.upstream_response_id = $4)) OR ($5 IS NOT NULL AND o.turn_id = $5) OR ($6 IS NOT NULL AND o.explicit_session_id = $6)) ORDER BY CASE WHEN $4 IS NOT NULL AND (o.turn_id = $4 OR o.upstream_response_id = $4) THEN 0 WHEN $5 IS NOT NULL AND o.turn_id = $5 THEN 1 ELSE 2 END, o.created_at DESC LIMIT 50",
            )
            .bind(&tenant_id)
            .bind(&principal_id)
            .bind(key.key_id.to_string())
            .bind(hints.parent_turn_id.as_deref())
            .bind(hints.turn_id.as_deref())
            .bind(hints.session_id.as_deref())
            .bind(now)
            .fetch_all(&mut **transaction)
            .await?
        } else {
            Vec::new()
        };
        let recent_candidates = sqlx::query(
            "SELECT o.id, o.cluster_id, CASE WHEN LENGTH(o.atom_hashes_json) <= 70000 THEN o.atom_hashes_json ELSE '[]' END AS atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.created_at <= $4 ORDER BY o.created_at DESC LIMIT 50",
        )
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(key.key_id.to_string())
        .bind(now)
        .fetch_all(&mut **transaction)
        .await?;
        for recent in recent_candidates {
            let recent_id: String = recent.try_get("id")?;
            let duplicate = candidates.iter().any(|candidate| {
                candidate
                    .try_get::<String, _>("id")
                    .is_ok_and(|candidate_id| candidate_id == recent_id)
            });
            if !duplicate {
                candidates.push(recent);
            }
        }

        let has_semantic_atoms = !atom_hashes.is_empty();
        debug_assert!(atom_hashes_json.len() <= MAX_CONVERSATION_FINGERPRINT_JSON_BYTES);
        let current_node_hashes: HashSet<&str> =
            nodes.iter().map(|node| node.node_hash.as_str()).collect();
        let mut selected: Option<ConversationSelection> = None;
        let mut candidate_selection: Option<ConversationSelection> = None;
        for row in candidates {
            let candidate_session: Option<String> = row.try_get("explicit_session_id")?;
            let candidate_turn: Option<String> = row.try_get("turn_id")?;
            let candidate_response: Option<String> = row.try_get("upstream_response_id")?;
            let candidate_branch: Option<String> = row.try_get("branch_id")?;
            let candidate_client: Option<String> = row.try_get("client_name")?;
            let candidate_leaf: Option<String> = row.try_get("leaf_node_hash")?;
            let previous_hashes_json: String = row.try_get("atom_hashes_json")?;
            let previous_hashes: Vec<String> =
                serde_json::from_str(&previous_hashes_json).unwrap_or_default();
            let has_previous_semantic_atoms = !previous_hashes.is_empty();
            let merkle_prefix = leaf.is_some()
                && (leaf.as_deref() == candidate_leaf.as_deref()
                    || candidate_leaf
                        .as_deref()
                        .is_some_and(|candidate| current_node_hashes.contains(candidate)));
            let (relation, confidence) =
                if leaf.as_deref() == candidate_leaf.as_deref() && leaf.is_some() {
                    (RelationKind::Retry, 980)
                } else if candidate_leaf
                    .as_deref()
                    .is_some_and(|candidate| current_node_hashes.contains(candidate))
                {
                    (RelationKind::Continues, 950)
                } else if has_semantic_atoms && has_previous_semantic_atoms {
                    infer_hash_relation(&previous_hashes, &atom_hashes)
                } else {
                    (RelationKind::Candidate, 0)
                };
            let created_at: i64 = row.try_get("created_at")?;
            let direct_parent = hints.parent_turn_id.is_some()
                && (hints.parent_turn_id.as_deref() == candidate_turn.as_deref()
                    || hints.parent_turn_id.as_deref() == candidate_response.as_deref());
            let same_turn =
                hints.turn_id.is_some() && hints.turn_id.as_deref() == candidate_turn.as_deref();
            let explicit_match = hints.session_id.is_some()
                && hints.session_id.as_deref() == candidate_session.as_deref();
            let conflicting_sessions =
                hints.session_id.is_some() && candidate_session.is_some() && !explicit_match;
            let exact_prefix = merkle_prefix
                || (has_semantic_atoms && has_previous_semantic_atoms && confidence >= 700);
            let recent_candidate = now.saturating_sub(created_at) <= 30 * 60 * 1_000;
            let same_client = client_name.is_some() && client_name == candidate_client.as_deref();
            let compaction_overlap = hints.compaction
                && recent_candidate
                && same_client
                && meaningful_atom_overlap(&previous_hashes, &atom_hashes);
            if direct_parent
                || same_turn
                || explicit_match
                || (exact_prefix && !conflicting_sessions)
                || (compaction_overlap && !conflicting_sessions)
            {
                let branch_changed = hints.branch_id.is_some()
                    && candidate_branch.is_some()
                    && hints.branch_id.as_deref() != candidate_branch.as_deref();
                let relation = if same_turn {
                    RelationKind::Retry
                } else if hints.compaction
                    || (explicit_match && atom_hashes.len() * 2 < previous_hashes.len())
                {
                    RelationKind::Compacts
                } else if direct_parent && branch_changed {
                    RelationKind::Branch
                } else if direct_parent {
                    RelationKind::Continues
                } else {
                    relation
                };
                let confidence = if direct_parent || same_turn {
                    995
                } else if explicit_match {
                    confidence.max(990)
                } else if compaction_overlap {
                    confidence.max(880)
                } else {
                    confidence
                };
                selected = Some(ConversationSelection {
                    observation_id: row.try_get("id")?,
                    cluster_id: row.try_get("cluster_id")?,
                    relation,
                    confidence,
                    direct_parent,
                    same_turn,
                    semantic_prefix: exact_prefix,
                    compaction_overlap,
                    client_match: same_client,
                    // A durable session id is sufficient to place observations in one
                    // cluster, but it does not prove that two adjacent requests are a
                    // continuation. Persist a directed edge only when the protocol names
                    // the parent/turn, the payload establishes a Merkle-prefix relation,
                    // or the client explicitly marks a compaction.
                    write_edge: direct_parent || same_turn || exact_prefix || hints.compaction,
                });
                if direct_parent || same_turn || explicit_match || exact_prefix {
                    break;
                }
            } else if candidate_selection.is_none()
                && has_semantic_atoms
                && has_previous_semantic_atoms
                && recent_candidate
                && same_client
                && !conflicting_sessions
            {
                candidate_selection = Some(ConversationSelection {
                    observation_id: row.try_get("id")?,
                    cluster_id: row.try_get("cluster_id")?,
                    relation: RelationKind::Candidate,
                    confidence: confidence.clamp(1, 699),
                    direct_parent: false,
                    same_turn: false,
                    semantic_prefix: false,
                    compaction_overlap: false,
                    client_match: true,
                    write_edge: true,
                });
            }
        }

        let cluster_id = if let Some(selection) = &selected {
            parse_uuid(selection.cluster_id.clone())?
        } else {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id.to_string())
            .bind(key.tenant_id.to_string())
            .bind(key.principal_id.to_string())
            .bind(hints.session_id.as_deref())
            .bind(now)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
            id
        };

        sqlx::query(
            "INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, leaf_node_hash, atom_hashes_json, explicit_session_id, client_name, created_at, inference_version, turn_id, parent_turn_id, branch_id, compaction) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 2, $10, $11, $12, $13)",
        )
        .bind(observation_id.to_string())
        .bind(cluster_id.to_string())
        .bind(request_id.to_string())
        .bind(key.key_id.to_string())
        .bind(leaf)
        .bind(atom_hashes_json)
        .bind(hints.session_id.as_deref())
        .bind(client_name)
        .bind(now)
        .bind(hints.turn_id.as_deref())
        .bind(hints.parent_turn_id.as_deref())
        .bind(hints.branch_id.as_deref())
        .bind(i64::from(hints.compaction))
        .execute(&mut **transaction)
        .await?;

        let edge_selection = selected
            .filter(|selection| selection.write_edge)
            .or(candidate_selection);
        let candidate_edge_increment = i64::from(
            edge_selection
                .as_ref()
                .is_some_and(|selection| selection.relation == RelationKind::Candidate),
        );
        if let Some(selection) = edge_selection {
            sqlx::query(
                "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 2, $8)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(cluster_id.to_string())
            .bind(selection.observation_id)
            .bind(observation_id.to_string())
            .bind(relation_name(selection.relation))
            .bind(selection.confidence)
            .bind(serde_json::json!({
                "explicit_session": hints.session_id.is_some(),
                "explicit_parent": selection.direct_parent,
                "same_turn": selection.same_turn,
                "branch": hints.branch_id.is_some(),
                "compaction": hints.compaction,
                "semantic_prefix": selection.semantic_prefix,
                "compaction_overlap": selection.compaction_overlap,
                "client_match": selection.client_match,
                "inference_version": 2
            }).to_string())
            .bind(now)
            .execute(&mut **transaction)
            .await?;
        }
        sqlx::query("UPDATE conversation_clusters SET created_at = CASE WHEN created_at > $1 THEN $1 ELSE created_at END, updated_at = CASE WHEN updated_at < $1 THEN $1 ELSE updated_at END, explicit_session_id = COALESCE(explicit_session_id, $2) WHERE id = $3")
            .bind(now)
            .bind(hints.session_id.as_deref())
            .bind(cluster_id.to_string())
            .execute(&mut **transaction)
            .await?;
        sqlx::query(
            "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, $3, $4, 1, $5) ON CONFLICT(key_id, cluster_id) DO UPDATE SET explicit_session_id = COALESCE(conversation_key_clusters.explicit_session_id, excluded.explicit_session_id), updated_at = CASE WHEN conversation_key_clusters.updated_at < excluded.updated_at THEN excluded.updated_at ELSE conversation_key_clusters.updated_at END, request_count = conversation_key_clusters.request_count + 1, candidate_edge_count = conversation_key_clusters.candidate_edge_count + excluded.candidate_edge_count",
        )
        .bind(key.key_id.to_string())
        .bind(cluster_id.to_string())
        .bind(hints.session_id.as_deref())
        .bind(now)
        .bind(candidate_edge_increment)
        .execute(&mut **transaction)
        .await?;
        if let Some(request_created_at) = request_created_at {
            let attached = sqlx::query(
                "UPDATE request_records SET conversation_cluster_id = $1 WHERE id = $2 AND created_at = $3",
            )
                .bind(cluster_id.to_string())
                .bind(&request_id)
                .bind(request_created_at)
                .execute(&mut **transaction)
                .await?;
            if attached.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            reclassify_request_session_in_transaction(transaction, parse_uuid(request_id.clone())?)
                .await?;
        }
        Ok(cluster_id)
    }

    pub async fn attach_conversation_upstream_response(
        &self,
        request_id: Uuid,
        upstream_response_id: &str,
    ) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        attach_conversation_upstream_response_in_transaction(
            &mut transaction,
            request_id,
            upstream_response_id,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn conversation_clusters(
        &self,
        key_id: Uuid,
        filter: ConversationListFilter,
    ) -> Result<Vec<ConversationClusterView>, AppError> {
        let limit = filter.limit.clamp(1, 100);
        let key_id = key_id.to_string();
        let rows = if let (Some(before_updated_at), Some(before_cluster_id)) =
            (filter.before_updated_at, filter.before_cluster_id)
        {
            sqlx::query(
                "SELECT p.cluster_id AS id, p.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p WHERE p.key_id = $1 AND (p.updated_at < $2 OR (p.updated_at = $2 AND p.cluster_id < $3)) ORDER BY p.updated_at DESC, p.cluster_id DESC LIMIT $4",
            )
            .bind(&key_id)
            .bind(before_updated_at)
            .bind(before_cluster_id.to_string())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT p.cluster_id AS id, p.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p WHERE p.key_id = $1 ORDER BY p.updated_at DESC, p.cluster_id DESC LIMIT $2",
            )
            .bind(&key_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| {
                Ok(ConversationClusterView {
                    cluster_id: parse_uuid(row.try_get("id")?)?,
                    explicit_session_id: row.try_get("explicit_session_id")?,
                    updated_at: row.try_get("updated_at")?,
                    request_count: row.try_get("request_count")?,
                    candidate_edge_count: row.try_get("candidate_edge_count")?,
                })
            })
            .collect()
    }

    pub async fn conversation_cluster_detail(
        &self,
        key_id: Uuid,
        cluster_id: Uuid,
        filter: ConversationDetailFilter,
    ) -> Result<ConversationClusterDetail, AppError> {
        let key_id = key_id.to_string();
        let cluster_id = cluster_id.to_string();
        let cluster_row = sqlx::query(
            "SELECT p.cluster_id AS id, p.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p WHERE p.key_id = $1 AND p.cluster_id = $2",
        )
        .bind(&key_id)
        .bind(&cluster_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let cluster = ConversationClusterView {
            cluster_id: parse_uuid(cluster_row.try_get("id")?)?,
            explicit_session_id: cluster_row.try_get("explicit_session_id")?,
            updated_at: cluster_row.try_get("updated_at")?,
            request_count: cluster_row.try_get("request_count")?,
            candidate_edge_count: cluster_row.try_get("candidate_edge_count")?,
        };
        let limit = filter.limit.clamp(1, 200);
        let request_rows = if let (Some(before_created_at), Some(before_request_id)) =
            (filter.before_created_at, filter.before_request_id)
        {
            sqlx::query(
                "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, currency, error_code, source_kind, provenance_kind, unlinked, archive_source, external_request_id FROM (SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, currency, error_code, 'live' AS source_kind, 'native' AS provenance_kind, CAST(0 AS BIGINT) AS unlinked, NULL AS archive_source, NULL AS external_request_id FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 UNION ALL SELECT archive_request_id AS id, source_started_at AS created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, CAST(0 AS BIGINT) AS cost_micros, NULL AS currency, error_code, 'session_archive' AS source_kind, 'archive_unlinked' AS provenance_kind, CAST(1 AS BIGINT) AS unlinked, source AS archive_source, external_request_id FROM session_archive_unlinked_requests WHERE key_id = $1 AND conversation_cluster_id = $2) requests WHERE created_at < $3 OR (created_at = $3 AND id < $4) ORDER BY created_at DESC, id DESC LIMIT $5",
            )
            .bind(&key_id)
            .bind(&cluster_id)
            .bind(before_created_at)
            .bind(before_request_id.to_string())
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, currency, error_code, source_kind, provenance_kind, unlinked, archive_source, external_request_id FROM (SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, currency, error_code, 'live' AS source_kind, 'native' AS provenance_kind, CAST(0 AS BIGINT) AS unlinked, NULL AS archive_source, NULL AS external_request_id FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 UNION ALL SELECT archive_request_id AS id, source_started_at AS created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, CAST(0 AS BIGINT) AS cost_micros, NULL AS currency, error_code, 'session_archive' AS source_kind, 'archive_unlinked' AS provenance_kind, CAST(1 AS BIGINT) AS unlinked, source AS archive_source, external_request_id FROM session_archive_unlinked_requests WHERE key_id = $1 AND conversation_cluster_id = $2) requests ORDER BY created_at DESC, id DESC LIMIT $3",
            )
            .bind(&key_id)
            .bind(&cluster_id)
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        };
        let mut requests = conversation_request_views(request_rows)?;
        let has_more = requests.len() > limit as usize;
        if has_more {
            requests.truncate(limit as usize);
        }
        let next_cursor = has_more.then(|| {
            let oldest = requests
                .last()
                .expect("a page with another row has at least one returned request");
            ConversationCursor {
                before_created_at: oldest.request.created_at,
                before_request_id: oldest.request.request_id,
            }
        });
        let page_request_ids: Vec<String> = requests
            .iter()
            .map(|request| request.request.request_id.to_string())
            .collect();
        requests.reverse();

        let edge_limit = (limit * 2).clamp(1, 400);
        let edge_rows = if page_request_ids.is_empty() {
            Vec::new()
        } else {
            let request_ids_json =
                serde_json::to_string(&page_request_ids).map_err(|_| AppError::Internal)?;
            let query = match self.backend {
                DatabaseBackend::PostgreSql => {
                    "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = $1 AND target_o.key_id = $2 AND (source_o.key_id = $3 OR source_o.id IS NULL) AND target_o.request_id IN (SELECT jsonb_array_elements_text(CAST($4 AS JSONB))) ORDER BY target_o.created_at ASC, target_o.id ASC, e.id ASC LIMIT $5"
                }
                DatabaseBackend::Sqlite => {
                    "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = $1 AND target_o.key_id = $2 AND (source_o.key_id = $3 OR source_o.id IS NULL) AND target_o.request_id IN (SELECT value FROM json_each($4)) ORDER BY target_o.created_at ASC, target_o.id ASC, e.id ASC LIMIT $5"
                }
            };
            sqlx::query(query)
                .bind(&cluster_id)
                .bind(&key_id)
                .bind(&key_id)
                .bind(request_ids_json)
                .bind(edge_limit + 1)
                .fetch_all(&self.pool)
                .await?
        };
        let mut edges = edge_rows
            .into_iter()
            .map(|row| {
                let from_request_id: Option<String> = row.try_get("from_request_id")?;
                let evidence: String = row.try_get("evidence_json")?;
                let confidence: i64 = row.try_get("confidence_millis")?;
                Ok(ConversationEdgeView {
                    from_request_id: from_request_id.map(parse_uuid).transpose()?,
                    to_request_id: parse_uuid(row.try_get("to_request_id")?)?,
                    relation: row.try_get("relation_kind")?,
                    confidence: confidence as f64 / 1_000.0,
                    evidence: serde_json::from_str(&evidence).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let edges_truncated = edges.len() > edge_limit as usize;
        if edges_truncated {
            edges.truncate(edge_limit as usize);
        }
        Ok(ConversationClusterDetail {
            cluster,
            requests,
            edges,
            has_more,
            next_cursor,
            edges_truncated,
        })
    }
}

pub(crate) async fn attach_conversation_upstream_response_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: Uuid,
    upstream_response_id: &str,
) -> Result<(), AppError> {
    let upstream_response_id = upstream_response_id.trim();
    if upstream_response_id.is_empty()
        || upstream_response_id.len() > 256
        || upstream_response_id.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "upstream response id must contain at most 256 non-control characters".into(),
        ));
    }
    let updated = sqlx::query(
        "UPDATE conversation_observations SET upstream_response_id = $1 WHERE request_id = $2",
    )
    .bind(upstream_response_id)
    .bind(request_id.to_string())
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

fn conversation_request_views(rows: Vec<AnyRow>) -> Result<Vec<ConversationRequestView>, AppError> {
    rows.into_iter()
        .map(|row| {
            Ok(ConversationRequestView {
                request: RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                },
                source: row.try_get("source_kind")?,
                provenance: row.try_get("provenance_kind")?,
                unlinked: row.try_get::<i64, _>("unlinked")? != 0,
                currency: row.try_get("currency")?,
                archive_source: row.try_get("archive_source")?,
                external_request_id: row.try_get("external_request_id")?,
            })
        })
        .collect()
}

fn infer_hash_relation(previous: &[String], current: &[String]) -> (RelationKind, i64) {
    let shared = previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == previous.len() && shared == current.len() {
        (RelationKind::Retry, 980)
    } else if shared == previous.len() && current.len() > previous.len() {
        (RelationKind::Continues, 950)
    // A single shared leading atom is commonly just the client's standard
    // system prompt. It is not sufficient ancestry evidence on its own.
    } else if shared >= 2 && shared + 1 >= previous.len().min(current.len()) {
        (RelationKind::Edit, 820)
    } else if shared >= 2 {
        (RelationKind::Branch, 720)
    } else {
        (RelationKind::Candidate, 350)
    }
}

/// Compaction replaces the beginning of a prompt, so a Merkle-prefix match is
/// impossible even when the client retained a real turn from the old context.
/// One exact atom outside the first/first position is useful evidence when it
/// is combined with the explicit compaction marker, the stable key, the same
/// client and the bounded recency window. Ignoring the first/first match avoids
/// merging unrelated requests merely because they share a standard system
/// prompt. With no such evidence the caller records only a candidate edge.
fn meaningful_atom_overlap(previous: &[String], current: &[String]) -> bool {
    if previous.is_empty() || current.is_empty() {
        return false;
    }
    let previous_positions: std::collections::HashMap<&str, usize> = previous
        .iter()
        .enumerate()
        .map(|(index, hash)| (hash.as_str(), index))
        .collect();
    current.iter().enumerate().any(|(current_index, hash)| {
        previous_positions
            .get(hash.as_str())
            .is_some_and(|previous_index| *previous_index > 0 || current_index > 0)
    })
}

fn bounded_atom_hashes(atoms: &[crate::conversation::SemanticAtom]) -> Vec<String> {
    atoms
        .iter()
        .take(MAX_CONVERSATION_FINGERPRINT_ATOMS)
        .map(|atom| atom.content_hash.clone())
        .collect()
}

fn relation_name(relation: RelationKind) -> &'static str {
    match relation {
        RelationKind::Continues => "continues",
        RelationKind::Retry => "retry",
        RelationKind::Edit => "edit",
        RelationKind::Branch => "branch",
        RelationKind::Compacts => "compacts",
        RelationKind::Subagent => "subagent",
        RelationKind::Candidate => "candidate",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_fingerprint_has_a_fixed_serialized_memory_bound() {
        let request = serde_json::json!({
            "messages": (0..(MAX_CONVERSATION_FINGERPRINT_ATOMS + 100))
                .map(|index| serde_json::json!({"role": "user", "content": index.to_string()}))
                .collect::<Vec<_>>()
        });
        let atoms = extract_atoms(&request);
        let fingerprint = bounded_atom_hashes(&atoms);
        let encoded = serde_json::to_vec(&fingerprint).unwrap();

        assert_eq!(fingerprint.len(), MAX_CONVERSATION_FINGERPRINT_ATOMS);
        assert!(encoded.len() <= MAX_CONVERSATION_FINGERPRINT_JSON_BYTES);
    }

    #[test]
    fn compaction_overlap_ignores_a_shared_leading_system_prompt() {
        let shared_system = "system".to_owned();
        let retained_turn = "retained".to_owned();
        assert!(!meaningful_atom_overlap(
            &[shared_system.clone(), "old".into()],
            &[shared_system.clone(), "new".into()]
        ));
        assert!(meaningful_atom_overlap(
            &[shared_system, retained_turn.clone()],
            &["summary".into(), retained_turn]
        ));
    }

    #[test]
    fn a_single_shared_leading_atom_never_becomes_a_merge_relation() {
        assert_eq!(
            infer_hash_relation(
                &["shared-system".into(), "old-user".into()],
                &["shared-system".into(), "unrelated-user".into()]
            ),
            (RelationKind::Candidate, 350)
        );
    }
}
