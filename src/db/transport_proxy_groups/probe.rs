use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Entry, Snapshot, TransportProxyGroups};
use crate::db::unix_millis;

const PROBE_INTERVAL: Duration = Duration::from_secs(60);

impl TransportProxyGroups {
    /// Probe configured exits on their own connection. A customer request never
    /// waits for this and never supplies the verdict.
    pub(crate) fn spawn_due_probes(&self) {
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
            tokio::spawn(async move {
                let switched = probe_current_member(&entry, &url, attempts).await;
                entry
                    .last_probe_millis
                    .store(unix_millis().max(0) as u64, Ordering::Release);
                entry.probe_inflight.store(false, Ordering::Release);
                if switched {
                    tracing::info!(
                        account_id = %entry.group.account_id,
                        "independent proxy probe moved the sticky exit"
                    );
                }
            });
        }
    }
}

async fn probe_current_member(entry: &Entry, target: &str, attempts: u8) -> bool {
    let state = entry.state.load(Ordering::Acquire);
    if state == 0 {
        return false;
    }
    let snapshot = Snapshot::decode(state);
    let Some(proxy) = entry.group.proxies.get(snapshot.selected).cloned() else {
        return false;
    };
    for _ in 0..attempts {
        if proxy_reaches(&proxy, target).await {
            return false;
        }
    }
    entry.advance_after_probe()
}

/// Any HTTP response means the proxy reached the configured address. Only a
/// failed socket, tunnel, DNS, or TLS handshake counts as unreachable. This
/// client carries no customer body and does not call the Responses API.
async fn proxy_reaches(proxy_url: &str, target: &str) -> bool {
    let Ok(proxy) = wreq::Proxy::all(proxy_url) else {
        return false;
    };
    let builder = wreq::Client::builder()
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
