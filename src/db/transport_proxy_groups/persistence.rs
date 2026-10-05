use super::*;

impl TransportProxyGroups {
    pub(crate) fn start(self: &Arc<Self>, database_url: &str) -> Result<(), sqlx::Error> {
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy(database_url)?;
        let groups = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(current) = groups.upgrade() {
                if current.refresh_managed(&pool).await.is_err() {
                    tracing::debug!("transport proxy configuration refresh deferred");
                }
                for entry in current.persistence_entries().unwrap_or_default() {
                    if entry.synchronize(&pool).await.is_err() {
                        tracing::debug!(account_id = %entry.group.account_id, "optional proxy selection persistence deferred");
                    }
                }
                drop(current);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        Ok(())
    }
}

impl Entry {
    pub(super) async fn synchronize(&self, pool: &AnyPool) -> Result<(), AppError> {
        let local = self.state.load(Ordering::Acquire);
        let row = sqlx::query(
            "SELECT a.credential_generation AS current_generation, s.group_version, s.group_fingerprint, s.credential_generation, s.base_index, s.selected_index, s.selection_generation FROM upstream_accounts a LEFT JOIN upstream_transport_proxy_selections s ON s.account_id = a.id WHERE a.id = $1",
        ).bind(self.group.account_id.to_string()).fetch_optional(pool).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let generation = u32::try_from(row.try_get::<i64, _>("current_generation")?)
            .map_err(|_| unavailable())?;
        self.observed_generation
            .fetch_max(u64::from(generation), Ordering::AcqRel);
        let version: Option<i64> = row.try_get("group_version")?;
        let fingerprint: Option<String> = row.try_get("group_fingerprint")?;
        if version.is_some_and(|version| version > self.group.version)
            || (version == Some(self.group.version)
                && fingerprint.as_deref() != Some(self.fingerprint.as_str()))
        {
            self.blocked.store(true, Ordering::Release);
            return Ok(());
        }
        let same_group = version == Some(self.group.version);
        let durable = if version.is_some() {
            Some(Snapshot {
                credential_generation: u32::try_from(
                    row.try_get::<i64, _>("credential_generation")?,
                )
                .map_err(|_| unavailable())?,
                epoch: u64::try_from(row.try_get::<i64, _>("selection_generation")?)
                    .map_err(|_| unavailable())?,
                base: usize::try_from(row.try_get::<i64, _>("base_index")?)
                    .map_err(|_| unavailable())?,
                selected: usize::try_from(row.try_get::<i64, _>("selected_index")?)
                    .map_err(|_| unavailable())?,
            })
        } else {
            None
        };
        let durable_state = durable.map(Snapshot::encode).transpose()?.unwrap_or(0);
        let expected = self.durable.swap(durable_state, Ordering::AcqRel);
        if local == 0 {
            if same_group
                && let Some(snapshot) = durable
                && snapshot.credential_generation == generation
                && snapshot.selected < self.group.proxies.len()
                && snapshot.base < self.group.proxies.len()
                && self
                    .state
                    .compare_exchange(0, durable_state, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                self.persisted.store(durable_state, Ordering::Release);
            }
            return Ok(());
        }
        let snapshot = Snapshot::decode(local);
        if snapshot.credential_generation != generation
            || local == self.persisted.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if same_group && let Some(previous) = durable {
            let conflicting = expected != 0 && expected != durable_state;
            let cold_conflict = expected == 0 && previous.selected != snapshot.base;
            if conflicting
                || cold_conflict
                || (previous.credential_generation == generation
                    && previous.base == snapshot.base
                    && previous.selected == snapshot.selected)
            {
                self.persisted.store(local, Ordering::Release);
                return Ok(());
            }
        }
        let next_epoch = durable.map_or(1, |previous| previous.epoch + 1);
        let next = Snapshot {
            epoch: next_epoch,
            ..snapshot
        }
        .encode()?;
        let result = if let Some(previous) = durable {
            sqlx::query(
                "UPDATE upstream_transport_proxy_selections SET group_version = $1, group_fingerprint = $2, credential_generation = $3, base_index = $4, selected_index = $5, selection_generation = $6 WHERE account_id = $7 AND group_version = $8 AND group_fingerprint = $9 AND credential_generation = $10 AND selection_generation = $11 AND selected_index = $12 AND EXISTS (SELECT 1 FROM upstream_accounts a WHERE a.id = $7 AND a.credential_generation = $3)",
            ).bind(self.group.version).bind(&self.fingerprint).bind(i64::from(generation))
                .bind(snapshot.base as i64).bind(snapshot.selected as i64).bind(next_epoch as i64)
                .bind(self.group.account_id.to_string()).bind(version).bind(fingerprint)
                .bind(i64::from(previous.credential_generation)).bind(previous.epoch as i64)
                .bind(previous.selected as i64).execute(pool).await?
        } else {
            sqlx::query(
                "INSERT INTO upstream_transport_proxy_selections (account_id, group_version, group_fingerprint, credential_generation, base_index, selected_index, selection_generation) SELECT id, $2, $3, $4, $5, $6, $7 FROM upstream_accounts WHERE id = $1 AND credential_generation = $4 ON CONFLICT (account_id) DO NOTHING",
            ).bind(self.group.account_id.to_string()).bind(self.group.version).bind(&self.fingerprint)
                .bind(i64::from(generation)).bind(snapshot.base as i64).bind(snapshot.selected as i64)
                .bind(next_epoch as i64).execute(pool).await?
        };
        if result.rows_affected() == 1 {
            self.durable.store(next, Ordering::Release);
            self.persisted.store(local, Ordering::Release);
        }
        Ok(())
    }
}
