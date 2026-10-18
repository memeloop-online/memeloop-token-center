//! Bounded reads of existing immutable context nodes. No archive objects or
//! tool bodies are read by the context-page query.
use super::*;

pub(crate) const CONTEXT_PAGE_NODES: i64 = 32;
const CONTEXT_INLINE_CHARS: i64 = 32_768;
pub(crate) const CONTEXT_CONTENT_CHARS: i64 = 16_384;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionContextScope {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub session_id: Uuid,
    pub request_id: Uuid,
    pub leaf: Option<String>,
}

pub(crate) struct SessionContextNode {
    pub hash: String,
    pub parent: Option<String>,
    pub atom: String,
    pub depth: i64,
    pub role: String,
    pub kind: String,
    pub content_chars: Option<i64>,
    pub content: Option<Value>,
}

pub(crate) struct SessionContextPage {
    pub nodes: Vec<SessionContextNode>,
    pub next_head: Option<String>,
}

impl Database {
    /// Authorize using retained tenant-scoped observations, including those
    /// whose credential has since been retired. Caller authentication is still
    /// performed on every request, including requests bearing a signed cursor.
    pub(crate) async fn session_context_scope(
        &self,
        tenant_external_id: Option<&str>,
        tenant_id: Option<Uuid>,
        key_id: Uuid,
        session_id: Uuid,
        request_id: Uuid,
    ) -> Result<SessionContextScope, AppError> {
        if tenant_external_id.is_none() && tenant_id.is_none() {
            return Err(AppError::Forbidden);
        }
        let row = sqlx::query(
            "SELECT c.tenant_id, o.leaf_node_hash FROM conversation_observations o \
             JOIN conversation_clusters c ON c.id = o.cluster_id \
             JOIN tenants t ON t.id = c.tenant_id \
             WHERE o.request_id = $1 AND o.key_id = $2 AND o.cluster_id = $3 \
             AND (CAST($4 AS TEXT) IS NULL OR t.external_id = $4) \
             AND (CAST($5 AS TEXT) IS NULL OR c.tenant_id = $5)",
        )
        .bind(request_id.to_string())
        .bind(key_id.to_string())
        .bind(session_id.to_string())
        .bind(tenant_external_id)
        .bind(tenant_id.map(|id| id.to_string()))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        Ok(SessionContextScope {
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            key_id,
            session_id,
            request_id,
            leaf: row.try_get("leaf_node_hash")?,
        })
    }

    /// `head` must be either the authorized leaf or an ancestor issued by the
    /// API's authenticated cursor. It is never accepted directly from a query.
    pub(crate) async fn session_context_page(
        &self,
        scope: &SessionContextScope,
        head: Option<&str>,
    ) -> Result<SessionContextPage, AppError> {
        let Some(head) = head else {
            return Ok(SessionContextPage {
                nodes: Vec::new(),
                next_head: None,
            });
        };
        let rows = sqlx::query(
            "WITH RECURSIVE path(node_hash, parent_hash, atom_hash, depth, step) AS (\
                 SELECT node_hash, parent_hash, atom_hash, depth, CAST(1 AS BIGINT) \
                 FROM context_nodes WHERE tenant_id = $1 AND node_hash = $2 \
                 UNION ALL \
                 SELECT n.node_hash, n.parent_hash, n.atom_hash, n.depth, p.step + 1 \
                 FROM context_nodes n JOIN path p ON n.node_hash = p.parent_hash \
                 AND n.depth = p.depth - 1 WHERE n.tenant_id = $1 AND p.step < $3\
             ) \
             SELECT p.node_hash, p.parent_hash, p.atom_hash, p.depth, a.role, a.kind, \
                    CASE WHEN a.role IN ('user', 'assistant') \
                         AND a.kind IN ('message', 'agent_message') \
                         THEN CAST(LENGTH(a.content_json) AS BIGINT) \
                         ELSE NULL END AS content_chars, \
                    CASE WHEN a.role IN ('user', 'assistant') \
                         AND a.kind IN ('message', 'agent_message') \
                         AND LENGTH(a.content_json) <= $4 \
                         THEN a.content_json ELSE NULL END AS content_json \
             FROM path p LEFT JOIN semantic_atoms a \
             ON a.tenant_id = $1 AND a.content_hash = p.atom_hash ORDER BY p.step DESC",
        )
        .bind(scope.tenant_id.to_string())
        .bind(head)
        .bind(CONTEXT_PAGE_NODES)
        .bind(CONTEXT_INLINE_CHARS)
        .fetch_all(&self.pool)
        .await?;
        let mut nodes = Vec::with_capacity(rows.len());
        for row in rows {
            let encoded: Option<String> = row.try_get("content_json")?;
            let node = SessionContextNode {
                hash: row.try_get("node_hash")?,
                parent: row.try_get("parent_hash")?,
                atom: row.try_get("atom_hash")?,
                depth: row.try_get("depth")?,
                role: row
                    .try_get::<Option<String>, _>("role")?
                    .ok_or_else(context_gap)?,
                kind: row
                    .try_get::<Option<String>, _>("kind")?
                    .ok_or_else(context_gap)?,
                content_chars: row.try_get("content_chars")?,
                content: encoded
                    .map(|value| serde_json::from_str(&value).map_err(|_| context_gap()))
                    .transpose()?,
            };
            if node.depth < 1
                || crate::conversation::digest_parts(node.parent.as_deref(), &node.atom)
                    != node.hash
                || (node.depth == 1) != node.parent.is_none()
            {
                return Err(context_gap());
            }
            nodes.push(node);
        }
        validate_context_page(head, &nodes)?;
        Ok(SessionContextPage {
            next_head: nodes.first().and_then(|node| node.parent.clone()),
            nodes,
        })
    }

    /// Only called with a node/atom pair from a verified item cursor. SQL
    /// returns one character range, never the entire potentially huge body.
    pub(crate) async fn session_context_content(
        &self,
        scope: &SessionContextScope,
        node: &str,
        atom: &str,
        offset: i64,
    ) -> Result<(String, i64), AppError> {
        if !(0..=i64::MAX - CONTEXT_CONTENT_CHARS - 1).contains(&offset) {
            return Err(AppError::BadRequest(
                "invalid context content offset".into(),
            ));
        }
        let row = sqlx::query(
            "SELECT SUBSTR(a.content_json, $4, $5) AS content, \
                    CAST(LENGTH(a.content_json) AS BIGINT) AS total_chars \
             FROM context_nodes n JOIN semantic_atoms a \
             ON a.tenant_id = n.tenant_id AND a.content_hash = n.atom_hash \
             WHERE n.tenant_id = $1 AND n.node_hash = $2 AND n.atom_hash = $3",
        )
        .bind(scope.tenant_id.to_string())
        .bind(node)
        .bind(atom)
        .bind(
            i32::try_from(offset + 1)
                .map_err(|_| AppError::BadRequest("context offset exceeds storage limit".into()))?,
        )
        .bind(CONTEXT_CONTENT_CHARS as i32)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(context_gap)?;
        Ok((row.try_get("content")?, row.try_get("total_chars")?))
    }

    /// Extract only declared message text. Mixed Anthropic tool results and
    /// media never leave the database through the default user-message reader.
    /// This uses the existing atom, not a second persisted copy of its text.
    pub(crate) async fn session_context_text(
        &self,
        scope: &SessionContextScope,
        node: &str,
        atom: &str,
        offset: i64,
    ) -> Result<(String, i64), AppError> {
        if !(0..=i64::MAX - CONTEXT_CONTENT_CHARS - 1).contains(&offset) {
            return Err(AppError::BadRequest("invalid context text offset".into()));
        }
        let extraction = match self.backend {
            DatabaseBackend::PostgreSql => "
                CASE jsonb_typeof(a.content_json::jsonb->'content')
                  WHEN 'string' THEN a.content_json::jsonb->>'content'
                  WHEN 'array' THEN COALESCE((
                    SELECT string_agg(part.value->>'text', '' ORDER BY part.ordinal)
                    FROM jsonb_array_elements(a.content_json::jsonb->'content') WITH ORDINALITY AS part(value, ordinal)
                    WHERE part.value->>'type' IN ('text', 'input_text', 'output_text')
                      AND jsonb_typeof(part.value->'text') = 'string'
                  ), '')
                  ELSE '' END",
            DatabaseBackend::Sqlite => "
                CASE json_type(a.content_json, '$.content')
                  WHEN 'text' THEN json_extract(a.content_json, '$.content')
                  WHEN 'array' THEN COALESCE((
                    SELECT group_concat(text, '') FROM (
                      SELECT json_extract(CASE WHEN part.type = 'object' THEN part.value ELSE '{}' END, '$.text') AS text
                      FROM json_each(a.content_json, '$.content') part
                      WHERE part.type = 'object'
                        AND json_extract(CASE WHEN part.type = 'object' THEN part.value ELSE '{}' END, '$.type') IN ('text', 'input_text', 'output_text')
                        AND json_type(CASE WHEN part.type = 'object' THEN part.value ELSE '{}' END, '$.text') = 'text'
                      ORDER BY CAST(part.key AS INTEGER)
                    )
                  ), '')
                  ELSE '' END",
        };
        let statement = format!(
            "WITH message AS (SELECT {extraction} AS text \
             FROM context_nodes n JOIN semantic_atoms a \
             ON a.tenant_id = n.tenant_id AND a.content_hash = n.atom_hash \
             WHERE n.tenant_id = $1 AND n.node_hash = $2 AND n.atom_hash = $3 \
               AND a.role IN ('user', 'assistant') AND a.kind IN ('message', 'agent_message')) \
             SELECT SUBSTR(text, $4, $5) AS content, CAST(LENGTH(text) AS BIGINT) AS total_chars FROM message"
        );
        let row =
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(scope.tenant_id.to_string())
                .bind(node)
                .bind(atom)
                .bind(i32::try_from(offset + 1).map_err(|_| {
                    AppError::BadRequest("context offset exceeds storage limit".into())
                })?)
                .bind(CONTEXT_CONTENT_CHARS as i32)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(context_gap)?;
        Ok((row.try_get("content")?, row.try_get("total_chars")?))
    }
}

fn context_gap() -> AppError {
    AppError::Conflict("retained context path is incomplete or inconsistent".into())
}

fn validate_context_page(head: &str, nodes: &[SessionContextNode]) -> Result<(), AppError> {
    if nodes.is_empty()
        || nodes.last().is_none_or(|node| node.hash != head)
        || nodes.len() > CONTEXT_PAGE_NODES as usize
        || nodes.windows(2).any(|pair| {
            pair[1].parent.as_deref() != Some(&pair[0].hash) || pair[1].depth != pair[0].depth + 1
        })
        || (nodes.len() < CONTEXT_PAGE_NODES as usize && nodes[0].parent.is_some())
    {
        return Err(context_gap());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_chunks_preserve_long_unicode_messages_without_tool_or_media_content() {
        use crate::db::requests::conversations::prefix_storage::tests::{key, observe};
        use serde_json::json;
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("text-reading.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let key = key(&database, "text-reading").await;
        let request = Uuid::now_v7();
        let text = "完整😀用户问题\n".repeat(8_000);
        let body = json!({"messages":[{"role":"user","content":[
            {"type":"text","text":text},
            {"type":"tool_result","content":"hidden-tool-body".repeat(10_000)},
            {"type":"image","source":{"data":"hidden-media".repeat(10_000)}},
            "unrecognized scalar",
            {"type":"text","text":"最后一段"}
        ]}]});
        let session = observe(
            &database,
            &key,
            &body,
            &crate::conversation::ConversationHints::default(),
            request,
            1,
            true,
        )
        .await;
        let scope = database
            .session_context_scope(Some("text-reading"), None, key.key_id, session, request)
            .await
            .unwrap();
        let page = database
            .session_context_page(&scope, scope.leaf.as_deref())
            .await
            .unwrap();
        let node = &page.nodes[0];
        assert!(
            node.content.is_none(),
            "large bodies are excluded from overview reads"
        );
        let expected = format!("{text}最后一段");
        let mut offset = 0;
        let mut collected = String::new();
        loop {
            let (chunk, total) = database
                .session_context_text(&scope, &node.hash, &node.atom, offset)
                .await
                .unwrap();
            assert_eq!(total, expected.chars().count() as i64);
            assert!(chunk.chars().count() <= CONTEXT_CONTENT_CHARS as usize);
            assert!(!chunk.contains("hidden-"));
            offset += chunk.chars().count() as i64;
            collected.push_str(&chunk);
            if offset == total {
                break;
            }
            assert!(!chunk.is_empty());
        }
        assert_eq!(collected, expected);
        assert!(
            database
                .session_context_text(&scope, &node.hash, "wrong-atom", 0)
                .await
                .is_err()
        );
    }

    fn node(hash: &str, parent: Option<&str>, depth: i64) -> SessionContextNode {
        SessionContextNode {
            hash: hash.into(),
            parent: parent.map(str::to_owned),
            atom: String::new(),
            depth,
            role: "user".into(),
            kind: "message".into(),
            content_chars: Some(0),
            content: None,
        }
    }

    #[test]
    fn missing_ancestors_are_not_reported_as_successful_end_of_history() {
        assert!(validate_context_page("head", &[node("head", Some("missing"), 2)]).is_err());
        assert!(validate_context_page("head", &[]).is_err());
        assert!(validate_context_page("other", &[node("head", None, 1)]).is_err());
        assert!(
            validate_context_page(
                "head",
                &[node("root", None, 1), node("head", Some("root"), 2)]
            )
            .is_ok()
        );
        assert!(
            validate_context_page(
                "head",
                &[node("root", None, 1), node("head", Some("wrong"), 2)]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn context_pages_preserve_long_history_and_resets_without_fetching_tools() {
        use crate::db::requests::conversations::prefix_storage::tests::{key, observe};
        use serde_json::json;
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("reading.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let key = key(&database, "reading-tenant").await;
        let request_id = Uuid::now_v7();
        let mut input = vec![
            json!({"role":"user","content":"intentional repeat"}),
            json!({"type":"function_call","name":"large_tool","arguments":"工具输出".repeat(50_000)}),
        ];
        input.extend(
            (0..1_050).map(|index| json!({"role":"user","content":format!("message {index}")})),
        );
        input.push(json!({"role":"user","content":"intentional repeat"}));
        let body = json!({"input":input});
        let hints = ConversationHints {
            session_id: Some("long-reading-session".into()),
            ..Default::default()
        };
        let cluster = observe(&database, &key, &body, &hints, request_id, 1, true).await;
        let reset_id = Uuid::now_v7();
        let reset_cluster = observe(
            &database,
            &key,
            &json!({"input":[{"role":"user","content":"new context after reset"}]}),
            &ConversationHints {
                compaction: true,
                ..hints
            },
            reset_id,
            2,
            true,
        )
        .await;
        assert_eq!(cluster, reset_cluster);

        let scope = database
            .session_context_scope(
                Some("reading-tenant"),
                None,
                key.key_id,
                cluster,
                request_id,
            )
            .await
            .unwrap();
        assert!(
            database
                .session_context_scope(
                    Some("another-tenant"),
                    None,
                    key.key_id,
                    cluster,
                    request_id
                )
                .await
                .is_err()
        );
        assert!(
            database
                .session_context_scope(None, Some(Uuid::now_v7()), key.key_id, cluster, request_id)
                .await
                .is_err()
        );
        assert!(
            database
                .session_context_scope(None, None, key.key_id, cluster, request_id)
                .await
                .is_err()
        );
        assert!(
            database
                .session_context_scope(
                    Some("reading-tenant"),
                    None,
                    Uuid::now_v7(),
                    cluster,
                    request_id
                )
                .await
                .is_err()
        );

        let mut head = scope.leaf.clone();
        let mut all = Vec::new();
        while head.is_some() {
            let page = database
                .session_context_page(&scope, head.as_deref())
                .await
                .unwrap();
            assert!(page.nodes.len() <= CONTEXT_PAGE_NODES as usize);
            head = page.next_head;
            all.extend(page.nodes);
        }
        all.sort_by_key(|node| node.depth);
        let expected =
            crate::conversation::build_prefix(&crate::conversation::extract_atoms(&body));
        assert_eq!(
            all.iter().map(|node| &node.hash).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|node| &node.node_hash)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            all.len(),
            1_053,
            "full paths, not capped inference fingerprints"
        );
        assert_ne!(
            all.first().unwrap().hash,
            all.last().unwrap().hash,
            "equal text at different positions is not deduplicated away"
        );
        let tool = &all[1];
        assert!(tool.content.is_none(), "no tool body in overview");
        assert!(
            tool.content_chars.is_none(),
            "overview does not even request tool length"
        );
        let (chunk, total) = database
            .session_context_content(&scope, &tool.hash, &tool.atom, 0)
            .await
            .unwrap();
        assert_eq!(chunk.chars().count(), CONTEXT_CONTENT_CHARS as usize);
        assert!(total > CONTEXT_CONTENT_CHARS);
        let (next, next_total) = database
            .session_context_content(&scope, &tool.hash, &tool.atom, CONTEXT_CONTENT_CHARS)
            .await
            .unwrap();
        assert_eq!(next_total, total);
        let full =
            serde_json::to_string(&crate::conversation::extract_atoms(&body)[1].content).unwrap();
        assert_eq!(
            format!("{chunk}{next}"),
            full.chars()
                .take((CONTEXT_CONTENT_CHARS * 2) as usize)
                .collect::<String>()
        );

        let reset_scope = database
            .session_context_scope(None, Some(key.tenant_id), key.key_id, cluster, reset_id)
            .await
            .unwrap();
        let reset_page = database
            .session_context_page(&reset_scope, reset_scope.leaf.as_deref())
            .await
            .unwrap();
        assert_eq!(reset_page.nodes.len(), 1);
        assert!(reset_page.next_head.is_none());

        // Retired keys do not erase retained history; a caller still needs
        // independent, currently valid management authentication at the API.
        sqlx::query("DELETE FROM key_records WHERE id = $1")
            .bind(key.key_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(
            database
                .session_context_scope(
                    Some("reading-tenant"),
                    None,
                    key.key_id,
                    cluster,
                    request_id
                )
                .await
                .is_ok()
        );
    }
}
