//! Deterministic queue contracts: gates are semaphore ownership, never sleeps.
use super::*;
use crate::api::proxy::persistence::{Persistence, WorkspaceRole};

#[tokio::test]
async fn legacy_try_workspace_retries_stay_behind_existing_raw_waiters() {
    let persistence = Persistence::default();
    let held = persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let held_other = persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut front = Box::pin(persistence.multipart_workspace(WorkspaceRole::RequestRaw));
    assert!(futures_util::poll!(&mut front).is_pending());
    // This is the old response-stage/offload attempt: it never joins FIFO.
    assert!(
        persistence
            .try_multipart_workspace(WorkspaceRole::ResponseStage)
            .is_err()
    );
    drop(held);
    let mut current = front.await.unwrap();
    for _ in 0..3 {
        let mut next_raw = Box::pin(persistence.multipart_workspace(WorkspaceRole::RequestRaw));
        assert!(futures_util::poll!(&mut next_raw).is_pending());
        assert!(
            persistence
                .try_multipart_workspace(WorkspaceRole::ResponseStage)
                .is_err()
        );
        drop(current);
        current = next_raw.await.unwrap();
        assert!(
            persistence
                .try_multipart_workspace(WorkspaceRole::ResponseStage)
                .is_err()
        );
    }
    assert!(
        persistence
            .render()
            .contains("archive_workspace_waiting{operation=\"response_stage\"} 0")
    );
    drop(current);
    drop(held_other);
    assert_eq!(persistence.upload_workspace.snapshot().0, 0);
}
