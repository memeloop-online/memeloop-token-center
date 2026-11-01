use super::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionIdentity {
    pub key_id: Uuid,
    pub session_id: Uuid,
}

pub(crate) struct SessionTaskOptions {
    pub timezone: String,
    pub timezone_offset_minutes: Option<i32>,
    pub include_subtrees: bool,
}

impl Default for SessionTaskOptions {
    fn default() -> Self {
        Self {
            timezone: "UTC".into(),
            timezone_offset_minutes: None,
            include_subtrees: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionNameUpdate {
    pub title: String,
    pub source: String,
    pub explicit_session_id: Option<String>,
    pub expected_revision: i64,
    #[serde(default)]
    pub expected_unnamed: bool,
    pub idempotency_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadParent {
    pub child_thread_id: Uuid,
    pub parent_thread_id: Uuid,
    pub evidence: String,
}

impl Database {
    pub(crate) async fn import_session_parents(
        &self,
        tenant: Uuid,
        parents: &[ThreadParent],
    ) -> Result<Value, AppError> {
        if parents.is_empty() {
            return Err(AppError::BadRequest("parents must be nonempty".into()));
        }
        let mut tx = self.pool.begin().await?;
        lock_session_tenant(&mut tx, tenant).await?;
        for parent in parents {
            if parent.child_thread_id == parent.parent_thread_id
                || !matches!(
                    parent.evidence.as_str(),
                    "thread_spawn_edges" | "source.subagent.thread_spawn"
                )
            {
                return Err(AppError::BadRequest(
                    "explicit thread-spawn evidence is required".into(),
                ));
            }
            let existing:Option<String>=sqlx::query_scalar("SELECT parent_thread_id FROM session_thread_parents WHERE tenant_id=$1 AND child_thread_id=$2")
                .bind(tenant.to_string()).bind(parent.child_thread_id.to_string()).fetch_optional(&mut *tx).await?;
            if existing.is_some_and(|id| id != parent.parent_thread_id.to_string()) {
                return Err(AppError::Conflict("contradictory parent thread".into()));
            }
            let cycle:i64=sqlx::query_scalar("WITH RECURSIVE ancestors(thread_id) AS (SELECT $2 UNION SELECT p.parent_thread_id FROM session_thread_parents p JOIN ancestors a ON p.child_thread_id=a.thread_id WHERE p.tenant_id=$1) SELECT COUNT(*) FROM ancestors WHERE thread_id=$3")
                .bind(tenant.to_string()).bind(parent.parent_thread_id.to_string()).bind(parent.child_thread_id.to_string()).fetch_one(&mut *tx).await?;
            if cycle > 0 {
                return Err(AppError::Conflict("parent thread cycle".into()));
            }
            sqlx::query("INSERT INTO session_thread_parents (tenant_id,child_thread_id,parent_thread_id,evidence) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING")
                .bind(tenant.to_string()).bind(parent.child_thread_id.to_string()).bind(parent.parent_thread_id.to_string()).bind(&parent.evidence).execute(&mut *tx).await?;
        }
        let rows=sqlx::query("WITH endpoints AS (SELECT DISTINCT o.explicit_session_id,o.key_id,o.cluster_id FROM conversation_observations o JOIN conversation_clusters c ON c.id=o.cluster_id WHERE c.tenant_id=$1 AND o.explicit_session_id IN (SELECT child_thread_id FROM session_thread_parents WHERE tenant_id=$1 UNION SELECT parent_thread_id FROM session_thread_parents WHERE tenant_id=$1)) SELECT DISTINCT p.child_thread_id,p.parent_thread_id,child.key_id,child.cluster_id,parent.key_id AS target_key_id,parent.cluster_id AS target_session_id,p.evidence FROM session_thread_parents p JOIN endpoints child ON child.explicit_session_id=p.child_thread_id JOIN endpoints parent ON parent.explicit_session_id=p.parent_thread_id WHERE p.tenant_id=$1 AND (child.key_id<>parent.key_id OR child.cluster_id<>parent.cluster_id)")
            .bind(tenant.to_string()).fetch_all(&mut *tx).await?;
        let mut parent_sets = BTreeMap::<_, BTreeMap<SessionIdentity, String>>::new();
        for row in rows {
            let source = SessionIdentity {
                key_id: parse_uuid(row.try_get("key_id")?)?,
                session_id: parse_uuid(row.try_get("cluster_id")?)?,
            };
            let target = SessionIdentity {
                key_id: parse_uuid(row.try_get("target_key_id")?)?,
                session_id: parse_uuid(row.try_get("target_session_id")?)?,
            };
            parent_sets
                .entry((
                    row.try_get::<String, _>("child_thread_id")?,
                    row.try_get::<String, _>("parent_thread_id")?,
                    source,
                ))
                .or_default()
                .insert(target, row.try_get("evidence")?);
        }
        let mut linked = 0;
        for ((_, _, source), targets) in parent_sets {
            validate_session_parents(&mut tx, tenant, &source, &targets.keys().cloned().collect())
                .await?;
            for (target, evidence) in targets {
                linked += sqlx::query("INSERT INTO session_relationships (tenant_id,key_id,session_id,target_key_id,target_session_id,kind,evidence,updated_at) VALUES ($1,$2,$3,$4,$5,'parent',$6,$7) ON CONFLICT DO NOTHING")
                    .bind(tenant.to_string()).bind(source.key_id.to_string()).bind(source.session_id.to_string())
                    .bind(target.key_id.to_string()).bind(target.session_id.to_string()).bind(evidence).bind(unix_millis())
                    .execute(&mut *tx).await?.rows_affected();
            }
        }
        tx.commit().await?;
        Ok(json!({"accepted":parents.len(),"linked":linked}))
    }
    pub(crate) async fn enqueue_unnamed_session_titles(
        &self,
        tenant: Uuid,
        identities: &[SessionIdentity],
    ) -> Result<Value, AppError> {
        let mut tx = self.pool.begin().await?;
        lock_session_tenant(&mut tx, tenant).await?;
        let mut queued = 0u64;
        let mut results = Vec::new();
        for identity in identities.iter().collect::<BTreeSet<_>>() {
            require_session(&mut tx, tenant, identity).await?;
            let named:i64=sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE key_id=$1 AND cluster_id=$2 AND session_name IS NOT NULL AND TRIM(session_name)<>''")
                .bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).fetch_one(&mut *tx).await?;
            if named > 0 {
                results.push(json!({"key_id":identity.key_id,"session_id":identity.session_id,"status":"named"}));
                continue;
            }
            let changed=sqlx::query("INSERT INTO session_generated_titles (tenant_id,key_id,session_id,next_attempt_at,updated_at) VALUES ($1,$2,$3,$4,$4) ON CONFLICT(tenant_id,key_id,session_id) DO UPDATE SET state='pending',revision=session_generated_titles.revision+1,attempts=0,next_attempt_at=excluded.next_attempt_at,updated_at=excluded.updated_at,lease_owner=NULL,lease_until=NULL,error_code=NULL WHERE session_generated_titles.title IS NULL AND session_generated_titles.title_source='generated' AND session_generated_titles.state NOT IN ('pending','running')")
                .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(unix_millis()).execute(&mut *tx).await?.rows_affected();
            queued += changed;
            let named:i64=sqlx::query_scalar("SELECT COUNT(*) FROM session_generated_titles WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND title IS NOT NULL")
                .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).fetch_one(&mut *tx).await?;
            results.push(json!({"key_id":identity.key_id,"session_id":identity.session_id,"status":if changed>0 {"queued"}else if named>0 {"named"}else{"already_queued"}}));
        }
        tx.commit().await?;
        Ok(json!({"queued":queued,"results":results}))
    }
    pub(crate) async fn session_task_tenant(&self, external: &str) -> Result<Uuid, AppError> {
        let id: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
            .bind(external)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        parse_uuid(id)
    }

    pub(crate) async fn resolve_session_threads(
        &self,
        tenant: Uuid,
        key: Option<Uuid>,
        threads: &[String],
    ) -> Result<Value, AppError> {
        if threads.is_empty()
            || threads
                .iter()
                .any(|thread| Uuid::parse_str(thread).is_err())
        {
            return Err(AppError::BadRequest(
                "explicit_session_ids must be a nonempty UUID list".into(),
            ));
        }
        let threads = threads
            .iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        for batch in threads.chunks(100) {
            let placeholders = (0..batch.len())
                .map(|index| format!("${}", index + 3))
                .collect::<Vec<_>>()
                .join(",");
            let statement = format!(
                "WITH matched AS (SELECT DISTINCT o.key_id,o.cluster_id,o.explicit_session_id FROM conversation_observations o JOIN conversation_clusters c ON c.id=o.cluster_id JOIN conversation_key_clusters p ON p.key_id=o.key_id AND p.cluster_id=o.cluster_id WHERE c.tenant_id=$1 AND ($2='' OR o.key_id=$2) AND o.explicit_session_id IN ({placeholders})) SELECT m.*,t.title,t.title_source,t.revision,(SELECT o.session_name FROM conversation_observations o WHERE o.key_id=m.key_id AND o.cluster_id=m.cluster_id AND o.session_name IS NOT NULL AND TRIM(o.session_name)<>'' ORDER BY o.created_at DESC,o.id DESC LIMIT 1) AS observed_title FROM matched m LEFT JOIN session_generated_titles t ON t.tenant_id=$1 AND t.key_id=m.key_id AND t.session_id=m.cluster_id ORDER BY m.key_id,m.cluster_id,m.explicit_session_id"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(tenant.to_string())
                .bind(key.map(|id| id.to_string()).unwrap_or_default());
            for thread in batch {
                query = query.bind(thread.as_str());
            }
            rows.extend(query.fetch_all(&self.pool).await?);
        }
        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            let identity = SessionIdentity {
                key_id: parse_uuid(row.try_get("key_id")?)?,
                session_id: parse_uuid(row.try_get("cluster_id")?)?,
            };
            let stored: Option<String> = row.try_get("title")?;
            let observed: Option<String> = row.try_get("observed_title")?;
            let source: Option<String> = row.try_get("title_source")?;
            let (title, source) = if stored.is_some() && source.as_deref() != Some("generated") {
                (stored, source.unwrap_or_else(|| "unknown".into()))
            } else if observed.is_some() {
                (observed, "observed".into())
            } else {
                (stored, source.unwrap_or_else(|| "unknown".into()))
            };
            sessions.push(json!({"tenant_id":tenant,"key_id":identity.key_id,"session_id":identity.session_id,"explicit_session_id":row.try_get::<String,_>("explicit_session_id")?,"title":title,"title_source":source,"title_revision":row.try_get::<Option<i64>,_>("revision")?.unwrap_or(0)}));
        }
        Ok(json!({"sessions":sessions}))
    }

    pub(crate) async fn update_session_name(
        &self,
        tenant: Uuid,
        identity: &SessionIdentity,
        update: SessionNameUpdate,
    ) -> Result<super::SessionTitleView, AppError> {
        if update.title.trim().is_empty()
            || update.title.chars().count() > 80
            || update.title.chars().any(char::is_control)
            || !matches!(update.source.as_str(), "manual" | "codex")
            || update.expected_revision < 0
            || update.idempotency_key.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
            })
        {
            return Err(AppError::BadRequest(
                "title must contain 1 to 80 printable characters; source must be manual or codex"
                    .into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_session_tenant(&mut tx, tenant).await?;
        require_session(&mut tx, tenant, identity).await?;
        if update.source == "codex" {
            let thread = update.explicit_session_id.as_deref().ok_or_else(|| {
                AppError::BadRequest("explicit_session_id is required for codex names".into())
            })?;
            let matched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE key_id = $1 AND cluster_id = $2 AND explicit_session_id = $3")
                .bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(thread).fetch_one(&mut *tx).await?;
            if matched == 0 {
                return Err(AppError::NotFound);
            }
        }
        if let Some(idempotency) = &update.idempotency_key {
            let prior=sqlx::query("SELECT title,title_source FROM session_generated_titles WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND name_sync_id=$4")
                .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(idempotency).fetch_optional(&mut *tx).await?;
            if let Some(prior) = prior {
                if prior.try_get::<Option<String>, _>("title")?.as_deref()
                    != Some(update.title.trim())
                    || prior.try_get::<String, _>("title_source")? != update.source
                {
                    return Err(AppError::Conflict(
                        "idempotency key was used for another name".into(),
                    ));
                }
                tx.commit().await?;
                return self
                    .session_title(tenant, identity.key_id, identity.session_id)
                    .await;
            }
        }
        if update.source == "codex" || update.expected_unnamed {
            let named: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE key_id = $1 AND cluster_id = $2 AND session_name IS NOT NULL AND TRIM(session_name) <> ''")
                .bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).fetch_one(&mut *tx).await?;
            if named > 0 {
                return Err(AppError::Conflict(
                    "existing observed name is preserved".into(),
                ));
            }
        }
        if update.expected_revision == 0 {
            sqlx::query("INSERT INTO session_generated_titles (tenant_id,key_id,session_id,revision,next_attempt_at,updated_at) VALUES ($1,$2,$3,0,$4,$4) ON CONFLICT DO NOTHING")
                .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(unix_millis()).execute(&mut *tx).await?;
        }
        let changed = sqlx::query("UPDATE session_generated_titles SET title=$4,title_source=$6,state='ready',revision=revision+1,updated_at=$5,lease_owner=NULL,lease_until=NULL,error_code=NULL,name_sync_id=$9 WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND revision=$7 AND ($6='manual' OR title_source <> 'manual') AND ($8=0 OR title IS NULL OR TRIM(title)='')")
            .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(update.title.trim()).bind(unix_millis()).bind(&update.source).bind(update.expected_revision)
            .bind(i64::from(update.expected_unnamed)).bind(update.idempotency_key)
            .execute(&mut *tx).await?.rows_affected();
        if changed == 0 {
            return Err(AppError::Conflict(
                "name revision changed or manual name is protected".into(),
            ));
        }
        tx.commit().await?;
        self.session_title(tenant, identity.key_id, identity.session_id)
            .await
    }

    pub(crate) async fn session_relationships(
        &self,
        tenant: Uuid,
        identity: &SessionIdentity,
        allowed_key: Option<Uuid>,
    ) -> Result<Value, AppError> {
        let mut tx = self.pool.begin().await?;
        require_session(&mut tx, tenant, identity).await?;
        let rows = sqlx::query("SELECT key_id,session_id,target_key_id,target_session_id,kind,evidence FROM session_relationships WHERE tenant_id = $1 AND ((key_id = $2 AND session_id = $3) OR (target_key_id = $2 AND target_session_id = $3)) AND ($4 = '' OR (key_id = $4 AND target_key_id = $4)) ORDER BY kind,key_id,session_id,target_key_id,target_session_id")
            .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(allowed_key.map(|key|key.to_string()).unwrap_or_default()).fetch_all(&mut *tx).await?;
        let relationships = rows.iter().map(|row| Ok(json!({"key_id":row.try_get::<String,_>("key_id")?,"session_id":row.try_get::<String,_>("session_id")?,"target":{"key_id":row.try_get::<String,_>("target_key_id")?,"session_id":row.try_get::<String,_>("target_session_id")?},"kind":row.try_get::<String,_>("kind")?,"evidence":row.try_get::<Option<String>,_>("evidence")?}))).collect::<Result<Vec<_>,AppError>>()?;
        tx.commit().await?;
        Ok(json!({"relationships":relationships}))
    }

    pub(crate) async fn change_session_relationship(
        &self,
        tenant: Uuid,
        identity: &SessionIdentity,
        target: &SessionIdentity,
        kind: &str,
        evidence: Option<&str>,
        remove: bool,
    ) -> Result<(), AppError> {
        if identity == target
            || !matches!(kind, "parent" | "related")
            || (kind == "parent" && evidence.is_none_or(|value| value.trim().is_empty()))
            || evidence
                .is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control))
        {
            return Err(AppError::BadRequest(
                "invalid relationship or missing parent evidence".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_session_tenant(&mut tx, tenant).await?;
        require_session(&mut tx, tenant, identity).await?;
        require_session(&mut tx, tenant, target).await?;
        let (source, target) = if kind == "related" && target < identity {
            (target, identity)
        } else {
            (identity, target)
        };
        if remove {
            if kind == "parent" {
                return Err(AppError::BadRequest(
                    "automatic parent evidence cannot be removed as a manual relation".into(),
                ));
            }
            sqlx::query("DELETE FROM session_relationships WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND target_key_id=$4 AND target_session_id=$5 AND kind=$6")
                .bind(tenant.to_string()).bind(source.key_id.to_string()).bind(source.session_id.to_string()).bind(target.key_id.to_string()).bind(target.session_id.to_string()).bind(kind).execute(&mut *tx).await?;
        } else {
            if kind == "parent" {
                validate_session_parents(
                    &mut tx,
                    tenant,
                    source,
                    &BTreeSet::from([target.clone()]),
                )
                .await?;
            }
            sqlx::query("INSERT INTO session_relationships (tenant_id,key_id,session_id,target_key_id,target_session_id,kind,evidence,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
                .bind(tenant.to_string()).bind(source.key_id.to_string()).bind(source.session_id.to_string()).bind(target.key_id.to_string()).bind(target.session_id.to_string()).bind(kind).bind(evidence).bind(unix_millis()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn analyze_session_tasks(
        &self,
        tenant: Uuid,
        identities: &[SessionIdentity],
        scope: &str,
        allowed_key: Option<Uuid>,
        options: &SessionTaskOptions,
    ) -> Result<Value, AppError> {
        if identities.is_empty()
            || !matches!(scope, "session" | "subtree" | "related")
            || (scope != "related" && identities.len() != 1)
        {
            return Err(AppError::BadRequest(
                "select one session/subtree or a nonempty related identity set".into(),
            ));
        }
        if options
            .timezone_offset_minutes
            .is_some_and(|offset| !(-840..=840).contains(&offset))
        {
            return Err(AppError::BadRequest(
                "timezone_offset_minutes must be between -840 and 840".into(),
            ));
        }
        if options.timezone_offset_minutes.is_some()
            && !matches!(options.timezone.as_str(), "UTC" | "Etc/UTC")
        {
            return Err(AppError::BadRequest(
                "choose an IANA timezone or a fixed offset, not both".into(),
            ));
        }
        let history_reader = self.history_reader().await?;
        let mut tx = self.pool.begin().await?;
        let postgres = matches!(self.backend, DatabaseBackend::PostgreSql);
        if postgres {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            let valid: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM pg_timezone_names WHERE name=$1")
                    .bind(&options.timezone)
                    .fetch_one(&mut *tx)
                    .await?;
            if valid == 0 {
                return Err(AppError::BadRequest(
                    "timezone must be a recognized IANA timezone".into(),
                ));
            }
        } else if !matches!(options.timezone.as_str(), "UTC" | "Etc/UTC") {
            return Err(AppError::BadRequest("IANA timezone analysis requires PostgreSQL; SQLite accepts UTC with timezone_offset_minutes".into()));
        }
        let mut selected = BTreeSet::new();
        for identity in identities.iter().collect::<BTreeSet<_>>() {
            if allowed_key.is_some_and(|key| key != identity.key_id) {
                return Err(AppError::NotFound);
            }
            require_session(&mut tx, tenant, identity).await?;
            selected.insert(identity.clone());
        }
        if scope == "subtree" || (scope == "related" && options.include_subtrees) {
            for root in identities.iter().collect::<BTreeSet<_>>() {
                let rows = sqlx::query("WITH RECURSIVE descendants(key_id,session_id) AS (SELECT $2,$3 UNION SELECT r.key_id,r.session_id FROM session_relationships r JOIN descendants d ON r.target_key_id=d.key_id AND r.target_session_id=d.session_id WHERE r.tenant_id=$1 AND r.kind='parent' AND ($4='' OR r.key_id=$4)) SELECT key_id,session_id FROM descendants")
                    .bind(tenant.to_string()).bind(root.key_id.to_string()).bind(root.session_id.to_string())
                    .bind(allowed_key.map(|key|key.to_string()).unwrap_or_default()).fetch_all(&mut *tx).await?;
                for row in rows {
                    selected.insert(SessionIdentity {
                        key_id: parse_uuid(row.try_get("key_id")?)?,
                        session_id: parse_uuid(row.try_get("session_id")?)?,
                    });
                }
            }
        }
        let selected = selected.into_iter().collect::<Vec<_>>();
        if let Some(client) = history_reader {
            tx.commit().await?;
            let tuples = selected
                .iter()
                .map(|identity| (identity.key_id, identity.session_id))
                .collect::<Vec<_>>();
            let mut activity = client
                .session_task_activity(
                    tenant,
                    &tuples,
                    &options.timezone,
                    options.timezone_offset_minutes,
                )
                .await?;
            let result = activity
                .as_object_mut()
                .ok_or_else(|| AppError::internal("history_session_task_activity"))?;
            result.insert("scope".into(), json!(scope));
            result.insert(
                "include_subtrees".into(),
                json!(scope == "subtree" || (scope == "related" && options.include_subtrees)),
            );
            result.insert("identities".into(), json!(selected));
            result.insert("timezone".into(), json!(options.timezone));
            result.insert(
                "timezone_offset_minutes".into(),
                json!(options.timezone_offset_minutes),
            );
            return Ok(activity);
        }
        let mut daily = BTreeMap::<i64, TaskUsage>::new();
        let mut day_starts = BTreeMap::<i64, i64>::new();
        let mut total = TaskUsage::default();
        let mut first: Option<i64> = None;
        let mut last: Option<i64> = None;
        for batch in selected.chunks(100) {
            let predicates = batch
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    format!(
                        "(key_id=${} AND session_id=${})",
                        index * 2 + 2,
                        index * 2 + 3
                    )
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            let zone_parameter = batch.len() * 2 + 2;
            let (local_day, day_start) = if postgres {
                let zone = if options.timezone_offset_minutes.is_some() {
                    format!("make_interval(mins => CAST(${zone_parameter} AS INTEGER))")
                } else {
                    format!("${zone_parameter}")
                };
                let date = format!(
                    "(TO_TIMESTAMP(created_at::double precision / 1000) AT TIME ZONE {zone})::date"
                );
                (
                    format!("CAST(FLOOR(EXTRACT(EPOCH FROM {date}) / 86400) AS BIGINT)"),
                    format!(
                        "CAST(EXTRACT(EPOCH FROM ({date}::timestamp AT TIME ZONE {zone})) * 1000 AS BIGINT)"
                    ),
                )
            } else {
                let shifted = format!("(created_at + ${zone_parameter} * 60000)");
                let day = format!(
                    "CAST(({shifted} - CASE WHEN {shifted}<0 THEN 86399999 ELSE 0 END) / 86400000 AS BIGINT)"
                );
                (
                    day.clone(),
                    format!("({day} * 86400000 - ${zone_parameter} * 60000)"),
                )
            };
            let statement = format!(
                "WITH activity AS (SELECT request_id,created_at,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,duration_ms,currency,cost_micros,CASE WHEN currency='' THEN 1 ELSE 0 END AS unknown FROM request_stats_facts WHERE tenant_id=$1 AND ({predicates}) UNION ALL SELECT archive_request_id,source_started_at,input_tokens,output_tokens,0,0,COALESCE(duration_ms,0),'',0,1 FROM (SELECT a.*,a.conversation_cluster_id AS session_id FROM session_archive_unlinked_requests a WHERE NOT EXISTS (SELECT 1 FROM request_stats_facts f WHERE f.tenant_id=a.tenant_id AND f.request_id=a.archive_request_id) AND NOT EXISTS (SELECT 1 FROM session_archive_correlations c WHERE c.tenant_id=a.tenant_id AND c.source=a.source AND c.external_request_id=a.external_request_id AND c.disposition='exact')) archive_only WHERE tenant_id=$1 AND ({predicates})) SELECT {local_day} AS day_bucket,MIN({day_start}) AS day_start_at,currency,CAST(COUNT(*) AS BIGINT) AS requests,MIN(created_at) AS first_at,MAX(created_at) AS last_at,CAST(SUM(input_tokens) AS BIGINT) AS input_tokens,CAST(SUM(output_tokens) AS BIGINT) AS output_tokens,CAST(SUM(cached_input_tokens) AS BIGINT) AS cached_input_tokens,CAST(SUM(cache_write_tokens) AS BIGINT) AS cache_write_tokens,CAST(SUM(duration_ms) AS BIGINT) AS request_duration_sum_ms,CAST(SUM(cost_micros) AS BIGINT) AS cost_micros,CAST(SUM(unknown) AS BIGINT) AS unknown_cost_requests FROM activity GROUP BY {local_day},currency ORDER BY day_bucket,currency"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(tenant.to_string());
            for identity in batch {
                query = query
                    .bind(identity.key_id.to_string())
                    .bind(identity.session_id.to_string());
            }
            query = if postgres && options.timezone_offset_minutes.is_none() {
                query.bind(&options.timezone)
            } else {
                query.bind(i64::from(options.timezone_offset_minutes.unwrap_or(0)))
            };
            for row in query.fetch_all(&mut *tx).await? {
                let day: i64 = row.try_get("day_bucket")?;
                let start: i64 = row.try_get("first_at")?;
                let end: i64 = row.try_get("last_at")?;
                first = Some(first.map_or(start, |value| value.min(start)));
                last = Some(last.map_or(end, |value| value.max(end)));
                day_starts.insert(day, row.try_get::<i64, _>("day_start_at")?);
                total.add(&row)?;
                daily.entry(day).or_default().add(&row)?;
            }
        }
        tx.commit().await?;
        let span = daily
            .first_key_value()
            .zip(daily.last_key_value())
            .map_or(0, |((start, _), (end, _))| end - start + 1);
        Ok(json!({
            "scope":scope,"include_subtrees":scope=="subtree" || (scope=="related" && options.include_subtrees),"identities":selected,
            "timezone":options.timezone,"timezone_offset_minutes":options.timezone_offset_minutes,
            "first_request_at":first,"last_request_at":last,"calendar_span_days":span,"active_days":daily.len(),
            "totals":total,"daily":daily.into_iter().map(|(day,usage)|json!({"day_bucket":day,"day_start_at":day_starts[&day],"usage":usage})).collect::<Vec<_>>(),
            "coverage":"completed_request_facts_and_archive_only"
        }))
    }
}

async fn validate_session_parents(
    tx: &mut Transaction<'_, Any>,
    tenant: Uuid,
    source: &SessionIdentity,
    targets: &BTreeSet<SessionIdentity>,
) -> Result<(), AppError> {
    let existing = sqlx::query("SELECT target_key_id,target_session_id FROM session_relationships WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND kind='parent'")
        .bind(tenant.to_string()).bind(source.key_id.to_string()).bind(source.session_id.to_string()).fetch_all(&mut **tx).await?;
    for row in existing {
        if !targets.contains(&SessionIdentity {
            key_id: parse_uuid(row.try_get("target_key_id")?)?,
            session_id: parse_uuid(row.try_get("target_session_id")?)?,
        }) {
            return Err(AppError::Conflict("contradictory parent evidence".into()));
        }
    }
    for target in targets {
        let cycle: i64 = sqlx::query_scalar("WITH RECURSIVE ancestors(key_id,session_id) AS (SELECT $2,$3 UNION SELECT r.target_key_id,r.target_session_id FROM session_relationships r JOIN ancestors a ON r.key_id=a.key_id AND r.session_id=a.session_id WHERE r.tenant_id=$1 AND r.kind='parent') SELECT COUNT(*) FROM ancestors WHERE key_id=$4 AND session_id=$5")
            .bind(tenant.to_string()).bind(target.key_id.to_string()).bind(target.session_id.to_string()).bind(source.key_id.to_string()).bind(source.session_id.to_string()).fetch_one(&mut **tx).await?;
        if cycle > 0 {
            return Err(AppError::Conflict("parent cycle".into()));
        }
    }
    Ok(())
}

async fn lock_session_tenant(tx: &mut Transaction<'_, Any>, tenant: Uuid) -> Result<(), AppError> {
    let changed = sqlx::query("UPDATE tenants SET external_id=external_id WHERE id=$1")
        .bind(tenant.to_string())
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

async fn require_session(
    tx: &mut Transaction<'_, Any>,
    tenant: Uuid,
    identity: &SessionIdentity,
) -> Result<(), AppError> {
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM conversation_key_clusters p JOIN conversation_clusters c ON c.id=p.cluster_id WHERE c.tenant_id=$1 AND p.key_id=$2 AND p.cluster_id=$3")
        .bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).fetch_one(&mut **tx).await?;
    if count == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

#[derive(Default, Serialize)]
struct TaskUsage {
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    request_duration_sum_ms: i64,
    unknown_cost_requests: i64,
    costs: BTreeMap<String, i64>,
}

impl TaskUsage {
    fn add(&mut self, row: &sqlx::any::AnyRow) -> Result<(), AppError> {
        self.requests += row.try_get::<i64, _>("requests")?;
        self.input_tokens += row.try_get::<i64, _>("input_tokens")?;
        self.output_tokens += row.try_get::<i64, _>("output_tokens")?;
        self.cached_input_tokens += row.try_get::<i64, _>("cached_input_tokens")?;
        self.cache_write_tokens += row.try_get::<i64, _>("cache_write_tokens")?;
        self.request_duration_sum_ms += row.try_get::<i64, _>("request_duration_sum_ms")?;
        let unknown: i64 = row.try_get("unknown_cost_requests")?;
        let currency: String = row.try_get("currency")?;
        self.unknown_cost_requests += unknown;
        if !currency.is_empty() {
            *self.costs.entry(currency).or_default() += row.try_get::<i64, _>("cost_micros")?;
        }
        Ok(())
    }
}
