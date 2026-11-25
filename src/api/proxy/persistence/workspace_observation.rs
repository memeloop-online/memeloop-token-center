//! Fixed operation labels; each shared workspace owner is counted once.
use crate::gateway_body::memory::ProxyMemoryReservation;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

#[derive(Clone, Copy)]
pub(crate) enum WorkspaceRole {
    RequestRaw,
    Normalized,
    ResponseStage,
    CarrierSpill,
}
impl WorkspaceRole {
    fn index(self) -> usize {
        self as usize
    }
    fn label(self) -> &'static str {
        match self {
            Self::RequestRaw => "request_raw",
            Self::Normalized => "normalized",
            Self::ResponseStage => "response_stage",
            Self::CarrierSpill => "carrier_spill",
        }
    }
}
#[derive(Default)]
struct Role {
    waiting: AtomicU64,
    active: AtomicU64,
    wait_us: AtomicU64,
    hold_us: AtomicU64,
}
#[derive(Default)]
pub(crate) struct WorkspaceRoles {
    roles: [Role; 4],
}
pub(crate) struct Waiting {
    roles: Arc<WorkspaceRoles>,
    role: WorkspaceRole,
    start: Instant,
}
pub(crate) struct WorkspaceGuard {
    _reservation: Arc<ProxyMemoryReservation>,
    reader: Arc<tokio::sync::Semaphore>,
    roles: Arc<WorkspaceRoles>,
    role: WorkspaceRole,
    start: Instant,
}
impl WorkspaceGuard {
    // A shared upload envelope has one sequential reader scratch region.
    // Clones cannot create independently uncharged reader allocations.
    pub(crate) async fn reader(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.reader
            .clone()
            .acquire_owned()
            .await
            .expect("workspace reader is never closed")
    }
}
impl WorkspaceRoles {
    pub(super) fn wait(self: &Arc<Self>, role: WorkspaceRole) -> Waiting {
        self.roles[role.index()]
            .waiting
            .fetch_add(1, Ordering::Relaxed);
        Waiting {
            roles: self.clone(),
            role,
            start: Instant::now(),
        }
    }
    pub(super) fn render(&self, out: &mut String) {
        use std::fmt::Write;
        for role in [
            WorkspaceRole::RequestRaw,
            WorkspaceRole::Normalized,
            WorkspaceRole::ResponseStage,
            WorkspaceRole::CarrierSpill,
        ] {
            let r = &self.roles[role.index()];
            for (name, value) in [
                ("waiting", r.waiting.load(Ordering::Relaxed)),
                ("active", r.active.load(Ordering::Relaxed)),
                ("wait_microseconds_total", r.wait_us.load(Ordering::Relaxed)),
                ("hold_microseconds_total", r.hold_us.load(Ordering::Relaxed)),
            ] {
                let _ = writeln!(
                    out,
                    "memeloop_token_center_archive_workspace_{name}{{operation=\"{}\"}} {value}",
                    role.label()
                );
            }
        }
    }
}
impl Waiting {
    pub(super) fn acquired(self, reservation: Arc<ProxyMemoryReservation>) -> Arc<WorkspaceGuard> {
        self.roles.roles[self.role.index()]
            .active
            .fetch_add(1, Ordering::Relaxed);
        Arc::new(WorkspaceGuard {
            _reservation: reservation,
            reader: Arc::new(tokio::sync::Semaphore::new(1)),
            roles: self.roles.clone(),
            role: self.role,
            start: Instant::now(),
        })
    }
}
impl Drop for Waiting {
    fn drop(&mut self) {
        let r = &self.roles.roles[self.role.index()];
        r.waiting.fetch_sub(1, Ordering::Relaxed);
        r.wait_us.fetch_add(
            self.start.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}
impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let r = &self.roles.roles[self.role.index()];
        r.active.fetch_sub(1, Ordering::Relaxed);
        r.hold_us.fetch_add(
            self.start.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn workspace_observation_counts_shared_owner_once_and_cancellation() {
        let persistence = Arc::new(super::super::Persistence::default());
        let a = persistence
            .multipart_workspace(WorkspaceRole::RequestRaw)
            .await
            .unwrap();
        let b = persistence
            .multipart_workspace(WorkspaceRole::Normalized)
            .await
            .unwrap();
        let duplicate = a.clone();
        let mut waiting = Box::pin(persistence.multipart_workspace(WorkspaceRole::ResponseStage));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        let metrics = persistence.render();
        assert!(metrics.contains("archive_workspace_active{operation=\"request_raw\"} 1"));
        assert!(metrics.contains("archive_workspace_waiting{operation=\"response_stage\"} 1"));
        drop(a);
        assert!(
            futures_util::poll!(&mut waiting).is_pending(),
            "shared SDK owner still retains envelope"
        );
        drop(duplicate);
        let c = waiting.await.unwrap();
        drop(c);
        drop(b);
        assert_eq!(persistence.upload_workspace.snapshot().0, 0);
        let a = persistence
            .multipart_workspace(WorkspaceRole::RequestRaw)
            .await
            .unwrap();
        let b = persistence
            .multipart_workspace(WorkspaceRole::Normalized)
            .await
            .unwrap();
        let mut waiting = Box::pin(persistence.multipart_workspace(WorkspaceRole::ResponseStage));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        drop(waiting);
        assert!(
            persistence
                .render()
                .contains("archive_workspace_waiting{operation=\"response_stage\"} 0")
        );
        drop(a);
        drop(b);
    }
}
