//! Source expectations are accumulated during the same RR baseline. The target
//! surface comparisons run before releasing that snapshot or delivery fence.
use super::*;
use crate::request_history::{
    ClickHouse, HistoryCursor, HistoryQuery, HistoryScope, HistorySnapshot, StatisticsAccumulator,
};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct BaselineParity {
    requests: u64,
    events: u64,
    credentials: BTreeMap<(Uuid, Uuid), Vec<[u8; 32]>>,
    daily:
        BTreeMap<crate::request_history::migration::DailyKey, crate::request_history::DailyBucket>,
    statistics: StatisticsAccumulator,
}
impl BaselineParity {
    pub(super) fn accumulate(&mut self, rows: &[HistorySnapshot]) -> Result<(), HistoryError> {
        for row in rows {
            if row.payload.is_none() {
                continue;
            }
            if row.identity.source == "event" {
                self.events += 1;
                continue;
            }
            self.requests += 1;
            self.credentials
                .entry((row.identity.tenant_id, row.identity.key_id))
                .or_default()
                .push(scope_fingerprint(row)?);
            crate::request_history::migration::add_daily(&mut self.daily, &row.row()?)?;
            self.statistics.accumulate(row)?;
        }
        Ok(())
    }
    pub(super) async fn verify(
        self,
        database: &Database,
        source: &mut Transaction<'_, Any>,
        client: &ClickHouse,
    ) -> Result<(), HistoryError> {
        let query = HistoryQuery {
            scope: HistoryScope::GlobalOperator,
            from_created_at: None,
            to_created_at: None,
            cursor: None,
            limit: 500,
            predicates: Vec::new(),
        };
        // Each source identity already passed canonical page readback. This
        // separate real FINAL enumeration rejects additional target identities.
        let mut global = query.clone();
        let mut requests = 0;
        loop {
            let mut rows = client.snapshots(&global).await?;
            rows.truncate(global.limit);
            if rows.is_empty() {
                break;
            }
            requests += rows.len() as u64;
            let last = rows.last().ok_or(HistoryError::Mismatch)?;
            global.cursor = Some(HistoryCursor {
                before_created_at: last.identity.created_at,
                before_id: last.identity.request_id,
            });
        }
        if requests != self.requests {
            return Err(HistoryError::Verification("baseline target inventory"));
        }
        let mut at = 0;
        let mut id = None;
        let mut events = 0;
        loop {
            let rows = client
                .event_snapshots(HistoryScope::GlobalOperator, at, id, 500)
                .await?;
            if rows.is_empty() {
                break;
            }
            events += rows.len() as u64;
            let last = rows.last().ok_or(HistoryError::Mismatch)?;
            at = last.identity.created_at;
            id = Some(last.identity.request_id);
        }
        if events != self.events {
            return Err(HistoryError::Verification("baseline event inventory"));
        }
        for ((tenant_id, key_id), mut expected) in self.credentials {
            let mut scoped = HistoryQuery {
                scope: HistoryScope::Key { tenant_id, key_id },
                ..query.clone()
            };
            let mut actual = Vec::with_capacity(expected.len());
            loop {
                let mut rows = client.snapshots(&scoped).await?;
                rows.truncate(scoped.limit);
                if rows.is_empty() {
                    break;
                }
                for row in &rows {
                    if row.identity.tenant_id != tenant_id || row.identity.key_id != key_id {
                        return Err(HistoryError::Verification("baseline credential membership"));
                    }
                    actual.push(scope_fingerprint(row)?);
                }
                let last = rows.last().ok_or(HistoryError::Mismatch)?;
                scoped.cursor = Some(HistoryCursor {
                    before_created_at: last.identity.created_at,
                    before_id: last.identity.request_id,
                });
            }
            expected.sort_unstable();
            actual.sort_unstable();
            if actual != expected {
                return Err(HistoryError::Verification("baseline credential scope"));
            }
            if client
                .list(&HistoryQuery {
                    limit: 1,
                    cursor: None,
                    ..scoped
                })
                .await?
                .requests
                .iter()
                .any(|payload| !payload["credential_identity"].is_null())
            {
                return Err(HistoryError::Verification("baseline credential redaction"));
            }
            if !client
                .snapshots(&HistoryQuery {
                    scope: HistoryScope::Key {
                        tenant_id,
                        key_id: Uuid::new_v4(),
                    },
                    limit: 1,
                    ..query.clone()
                })
                .await?
                .is_empty()
            {
                return Err(HistoryError::Verification("baseline absent credential"));
            }
        }
        if !client
            .snapshots(&HistoryQuery {
                scope: HistoryScope::EmptyTenant,
                ..query.clone()
            })
            .await?
            .is_empty()
        {
            return Err(HistoryError::Verification("baseline absent tenant"));
        }
        let daily = client
            .daily(&query)
            .await?
            .into_iter()
            .map(|row| (crate::request_history::migration::daily_key(&row), row))
            .collect::<BTreeMap<_, _>>();
        if daily != self.daily {
            return Err(HistoryError::Verification("baseline daily aggregates"));
        }
        // Retain the original production PG statistics query on this exact RR
        // socket. Comparing two uses of the shared accumulator is not parity.
        let days: std::collections::BTreeSet<_> = self.daily.keys().map(|key| key.0).collect();
        for day in days {
            let from = day
                .checked_mul(86_400_000)
                .ok_or(HistoryError::Invalid("baseline statistics date"))?;
            let to = from
                .checked_add(86_399_999)
                .ok_or(HistoryError::Invalid("baseline statistics date"))?;
            let expected = database
                .source_history_stats_with_executor(
                    None,
                    &crate::db::StatsFilter {
                        from_created_at: Some(from),
                        to_created_at: Some(to),
                        ..Default::default()
                    },
                    &mut **source,
                )
                .await?;
            let actual = client
                .statistics(&HistoryQuery {
                    from_created_at: Some(from),
                    to_created_at: Some(to),
                    ..query.clone()
                })
                .await?;
            if serde_json::to_value(expected)? != serde_json::to_value(actual)? {
                return Err(HistoryError::Verification("baseline production statistics"));
            }
        }
        if serde_json::to_value(client.statistics(&query).await?)?
            != serde_json::to_value(self.statistics.finish()?)?
        {
            return Err(HistoryError::Verification("baseline statistics"));
        }
        Ok(())
    }
}

fn scope_fingerprint(snapshot: &HistorySnapshot) -> Result<[u8; 32], HistoryError> {
    use sha2::Digest;
    Ok(sha2::Sha256::digest(serde_json::to_vec(&serde_json::to_value(snapshot)?)?).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[tokio::test]
    async fn baseline_scope_parity_rejects_equal_count_swapped_key_payload() {
        let expected = HistorySnapshot {
            identity: HistoryIdentity {
                tenant_id: Uuid::now_v7(),
                key_id: Uuid::now_v7(),
                request_id: Uuid::now_v7(),
                source: "native".into(),
                created_at: 1,
            },
            revision: 1,
            filter_cost_micros: None,
            payload: Some(serde_json::json!({"model":"fixture","protocol":"openai"})),
            statistics: None,
            read_model: Value::Null,
        };
        let mut wrong = expected.clone();
        wrong.identity.key_id = Uuid::now_v7();
        let response=serde_json::json!({"snapshot":serde_json::to_string(&wrong).unwrap(),"snapshot_hash":crate::request_history::canonical_hash(&wrong).unwrap()}).to_string()+"\n";
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let router = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let seen = seen.clone();
                let response = response.clone();
                async move {
                    if matches!(seen.fetch_add(1, Ordering::SeqCst), 1 | 2) {
                        String::new()
                    } else {
                        response
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            root.path().join("scope.db").display()
        ))
        .await
        .unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        let client = ClickHouse::new(
            db.http_clients
                .handle(crate::http_settings::HttpPurpose::General),
            &endpoint,
            "default".into(),
            "default".into(),
            String::new(),
        )
        .unwrap();
        let mut parity = BaselineParity::default();
        parity.accumulate(&[expected]).unwrap();
        assert!(parity.verify(&db, &mut tx, &client).await.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "equal global count must still reach and reject the scoped wrong-key result"
        );
        tx.rollback().await.unwrap();
        server.abort();
    }
}
