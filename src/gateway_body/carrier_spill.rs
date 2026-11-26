use super::*;

/// Exactly one owned invocation of the original spill writer. Dropping the
/// capture observer never aborts an already submitted PUT.
pub(super) struct CaptureSpill {
    task: Option<tokio::task::JoinHandle<Result<CompletedSpill, AppError>>>,
    archive: ArchiveStore,
    activity: Option<crate::api::proxy::persistence::ArchiveActivity>,
    runtime: tokio::runtime::Handle,
}

impl CaptureSpill {
    pub(super) fn start(
        attempt: PreparedSpill,
        persistence: &Arc<crate::api::proxy::persistence::Persistence>,
    ) -> Self {
        let activity = persistence.register_archive();
        let archive = attempt.archive.clone();
        Self {
            task: Some(tokio::spawn(attempt.execute())),
            archive,
            activity: Some(activity),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub(super) async fn completed(&mut self) -> Result<CompletedSpill, AppError> {
        let result = self.task.as_mut().expect("one active spill").await;
        self.task.take();
        result.map_err(|_| AppError::storage("carrier_spill_task_failed"))?
    }

    pub(super) fn handoff(mut self, source: Arc<BodyCarrier>) {
        let task = self.task.take().expect("one active spill");
        let activity = self.activity.take();
        self.runtime.spawn(async move {
            let _activity = activity;
            match task.await {
                Ok(Ok(completed)) => {
                    if let Err(error) = source.apply_spill(completed) {
                        observe_failure(&error);
                    }
                }
                Ok(Err(error)) => observe_failure(&error),
                Err(_) => observe_failure(&AppError::storage("carrier_spill_task_failed")),
            }
        });
    }
}

impl Drop for CaptureSpill {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        let archive = self.archive.clone();
        let activity = self.activity.take();
        // Capture was cancelled before returning a sealed source. Only a
        // confirmed complete exact object can enter the existing release path.
        // Errors/unknown ACKs retain the active receipt; no prefix is sealed.
        self.runtime.spawn(async move {
            let _activity = activity;
            match task.await {
                Ok(Ok(completed)) => {
                    if let Err(error) = archive.release_carrier_object(&completed.owner).await {
                        observe_failure(&error);
                    }
                }
                Ok(Err(error)) => observe_failure(&error),
                Err(_) => observe_failure(&AppError::storage("carrier_spill_task_failed")),
            }
        });
    }
}

pub(super) fn observe_failure(error: &AppError) {
    tracing::warn!(
        phase = "request_carrier_spill",
        error_category = error.diagnostic_category(),
        "request spill retained its original source or active receipt"
    );
}

// The existing spill writer can own one exact attempt without borrowing the
// capture loop. The original bytes/credit and mutex generation survive I/O.
pub(super) struct PreparedSpill {
    pub(super) archive: ArchiveStore,
    pub(super) index: usize,
    pub(super) bytes: Bytes,
    pub(super) owner: CarrierObjectOwner,
    pub(super) locator: String,
    pub(super) workspace: Option<Arc<crate::api::proxy::persistence::WorkspaceGuard>>,
    pub(super) guard: tokio::sync::OwnedMutexGuard<()>,
}

pub(super) struct CompletedSpill {
    pub(super) index: usize,
    pub(super) bytes: Bytes,
    pub(super) owner: CarrierObjectOwner,
    pub(super) staged: StagedArchiveObject,
    pub(super) _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl PreparedSpill {
    pub(super) async fn execute(self) -> Result<CompletedSpill, AppError> {
        let Self {
            archive,
            index,
            bytes,
            owner,
            locator,
            workspace,
            guard,
        } = self;
        archive.begin_carrier_object(&owner).await?;
        let staged = if bytes.len() <= crate::archive::ARCHIVE_MULTIPART_PART_BYTES {
            // One owned segment already fits the existing multipart envelope.
            // A single PUT retains these exact Bytes until its ACK; uncertain
            // completion leaves RAM and the active receipt with this carrier.
            let payload = Bytes::from_owner(WorkspaceWrite {
                bytes: bytes.clone(),
                _workspace: workspace.map(|guard| guard as Arc<dyn Send + Sync>),
            });
            archive.put(&locator, payload).await?;
            StagedArchiveObject {
                object_locator: locator,
                size_bytes: bytes.len() as u64,
                blake3_digest: blake3::hash(&bytes).to_hex().to_string(),
            }
        } else {
            let mut writer = archive.start_writer(&locator).await?;
            if let Some(workspace) = workspace {
                writer.retain_workspace(workspace);
            }
            if let Err(error) = writer.write(bytes.clone()).await {
                let _ = writer.abort().await;
                return Err(error);
            }
            let staged = writer.finish_staged().await?;
            staged
        };
        if staged.size_bytes != bytes.len() as u64
            || staged.blake3_digest != blake3::hash(&bytes).to_hex().as_str()
        {
            return Err(AppError::storage("carrier_stage_identity_mismatch"));
        }
        Ok(CompletedSpill {
            index,
            bytes,
            owner,
            staged,
            _guard: guard,
        })
    }
}
