use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::AnyPool;

use super::{Entry, Snapshot, TransportProxyGroups};
use crate::db::unix_millis;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_INTERVAL: Duration = Duration::from_secs(60);

enum ProbeOutcome {
    Reachable,
    Unreachable,
    Skipped,
}

impl TransportProxyGroups {
    /// Probe configured exits on their own connection. A customer request never
    /// waits for this and never supplies the verdict.
    pub(crate) fn request_account_probe(&self, account_id: uuid::Uuid) -> bool {
        let requested = |entry: &Entry| {
            entry.probe_requested.store(true, Ordering::Release);
        };
        if let Ok(managed) = self.managed.read()
            && let Some(item) = managed.get(&account_id)
            && let Some(entry) = &item.entry
        {
            requested(entry);
            return true;
        }
        if let Some(entry) = self.groups.get(&account_id) {
            requested(&entry);
            return true;
        }
        false
    }

    pub(crate) fn spawn_due_probes(&self, pool: &AnyPool) {
        let Ok(managed) = self.managed.read() else {
            return;
        };
        let now = unix_millis().max(0) as u64;
        for item in managed.values() {
            let Some(entry) = item.entry.clone() else {
                continue;
            };
            let url = entry.probe_url.lock().ok().and_then(|slot| slot.clone());
            let Some(url) = url else {
                entry.probe_requested.store(false, Ordering::Release);
                continue;
            };
            let requested = entry.probe_requested.load(Ordering::Acquire);
            let last = entry.last_probe_millis.load(Ordering::Acquire);
            let stale = now.saturating_sub(last) >= PROBE_INTERVAL.as_millis() as u64;
            if !requested && !stale {
                continue;
            }
            if entry
                .probe_inflight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            entry.probe_requested.store(false, Ordering::Release);
            let attempts = entry.probe_attempts.load(Ordering::Acquire).clamp(1, 8);
            let pool = pool.clone();
            let account_id = entry.group.account_id;
            tokio::spawn(async move {
                let outcome = probe_current_member(&entry, &url, attempts).await;
                entry
                    .last_probe_millis
                    .store(unix_millis().max(0) as u64, Ordering::Release);
                entry.probe_inflight.store(false, Ordering::Release);
                let status = match outcome {
                    ProbeOutcome::Reachable => "healthy",
                    ProbeOutcome::Unreachable => {
                        tracing::info!(
                            %account_id,
                            "independent proxy probe moved the sticky exit"
                        );
                        "unhealthy"
                    }
                    ProbeOutcome::Skipped => return,
                };
                if let Err(error) = sqlx::query(
                    "UPDATE transport_proxy_groups SET last_probe_status = $1, last_probe_checked_at = $2 WHERE id = (SELECT group_id FROM transport_proxy_bindings WHERE account_id = $3)",
                )
                .bind(status)
                .bind(unix_millis())
                .bind(account_id.to_string())
                .execute(&pool)
                .await
                {
                    tracing::warn!(%account_id, %error, "independent proxy probe result was not stored");
                }
            });
        }
    }
}

async fn probe_current_member(entry: &Entry, target: &str, attempts: u8) -> ProbeOutcome {
    let state = entry.state.load(Ordering::Acquire);
    if state == 0 {
        return ProbeOutcome::Skipped;
    }
    let snapshot = Snapshot::decode(state);
    let Some(proxy) = entry.group.proxies.get(snapshot.selected).cloned() else {
        return ProbeOutcome::Skipped;
    };
    for _ in 0..attempts {
        if proxy_reaches(&proxy, target).await {
            return ProbeOutcome::Reachable;
        }
    }
    entry.advance_after_probe(state);
    ProbeOutcome::Unreachable
}

/// Any HTTP response means the proxy reached the configured address. Only a
/// failed socket, tunnel, DNS, or TLS handshake counts as unreachable. This
/// client carries no customer body and does not call the Responses API.
async fn proxy_reaches(proxy_url: &str, target: &str) -> bool {
    let Ok(proxy) = wreq::Proxy::all(proxy_url) else {
        return false;
    };
    // Only the independent probe has a clock. A blackholed exit must finish
    // probing so later requests can use a reachable member.
    let builder = wreq::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .redirect(wreq::redirect::Policy::none())
        .retry(wreq::retry::Policy::never())
        .no_proxy()
        .proxy(proxy)
        .tcp_keepalive(None::<Duration>)
        .tcp_keepalive_interval(None::<Duration>)
        .tcp_keepalive_retries(None::<u32>);
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    let builder = builder.tcp_user_timeout(None::<Duration>);
    let Ok(client) = builder.build() else {
        return false;
    };
    match client.get(target).send().await {
        Ok(response) => {
            tracing::debug!(
                status = response.status().as_u16(),
                "independent proxy probe reached the configured address"
            );
            true
        }
        Err(error) => {
            tracing::debug!(
                connect = error.is_connect(),
                proxy_connect = error.is_proxy_connect(),
                dns = error.is_dns(),
                tls = error.is_tls(),
                timed_out = error.is_timeout(),
                "independent proxy probe did not reach the configured address"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn blackholed_independent_probe_finishes_without_timing_out_a_customer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("socks5h://{}", listener.local_addr().unwrap());
        let probe = tokio::spawn(async move { proxy_reaches(&proxy, "https://example.com").await });
        let (_silent_socket, _) = listener.accept().await.unwrap();
        tokio::time::advance(PROBE_TIMEOUT).await;
        assert!(!probe.await.unwrap());
    }
}
