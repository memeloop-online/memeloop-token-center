//! Durable, encrypted response capture. Object storage is never on the
//! client-facing delivery path; only a short, bounded database ACK is.
mod cipher;
mod producer;
mod upload;

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::error::AppError;

pub(crate) use producer::BufferedArchive;
#[cfg(test)]
pub(crate) use producer::capture_buffered;
#[cfg(test)]
pub(crate) use producer::encrypt_buffered;
#[cfg(test)]
pub(crate) use producer::fail_next_append_for_test;
#[cfg(test)]
pub(crate) use producer::pause_next_begin_ack_for_test;
pub(crate) use producer::{ResponseArchiveProducer, mark_gap};
pub(crate) use upload::run;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BufferedArchivePurpose {
    Request,
    Response,
}

impl BufferedArchivePurpose {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
        }
    }

    pub(crate) fn staging(self) -> crate::archive_staging::ArchiveStagingPurpose {
        match self {
            Self::Request => crate::archive_staging::ArchiveStagingPurpose::Request,
            Self::Response => crate::archive_staging::ArchiveStagingPurpose::Response,
        }
    }
}

pub(crate) const CHUNK_BYTES: usize = 64 * 1024;
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

struct OwnedTask<T: Send + 'static> {
    task: Option<tokio::task::JoinHandle<Result<T, AppError>>>,
    stage: &'static str,
    active: Option<Arc<AtomicBool>>,
}

impl<T> OwnedTask<T>
where
    T: Send + 'static,
{
    fn spawn(
        operation: impl Future<Output = Result<T, AppError>> + Send + 'static,
        stage: &'static str,
        active: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            task: Some(tokio::spawn(operation)),
            stage,
            active,
        }
    }

    async fn wait(&mut self) -> Option<Result<T, AppError>> {
        let result = self.task.as_mut()?.await;
        self.task.take();
        match result {
            Ok(result) => Some(result),
            Err(error) => {
                log_task_failure(self.stage, &error, false);
                None
            }
        }
    }
}

impl<T: Send + 'static> Drop for OwnedTask<T> {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.store(false, Ordering::Release);
        }
        let Some(task) = self.task.take() else {
            return;
        };
        let stage = self.stage;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                match task.await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!(
                        stage,
                        error_category = error.diagnostic_category(),
                        "late archive database task failed"
                    ),
                    Err(error) => log_task_failure(stage, &error, true),
                }
            });
        }
    }
}

fn log_task_failure(stage: &'static str, error: &tokio::task::JoinError, late: bool) {
    tracing::error!(
        stage,
        late,
        task_cancelled = error.is_cancelled(),
        task_panicked = error.is_panic(),
        "archive database task failed"
    );
}

/// Observe an owned operation for a bounded amount of time without cancelling
/// it. Database futures may own a transaction whose asynchronous rollback must
/// finish before SQLx can safely return the connection to the pool.
async fn await_owned<T>(
    deadline: std::time::Duration,
    operation: impl Future<Output = Result<T, AppError>> + Send + 'static,
    stage: &'static str,
) -> Option<Result<T, AppError>>
where
    T: Send + 'static,
{
    await_owned_with_active(deadline, operation, stage, None).await
}

async fn await_owned_with_active<T>(
    deadline: std::time::Duration,
    operation: impl Future<Output = Result<T, AppError>> + Send + 'static,
    stage: &'static str,
    active: Option<Arc<AtomicBool>>,
) -> Option<Result<T, AppError>>
where
    T: Send + 'static,
{
    await_owned_until(tokio::time::sleep(deadline), operation, stage, active).await
}

async fn await_owned_until<T>(
    deadline: impl Future<Output = ()>,
    operation: impl Future<Output = Result<T, AppError>> + Send + 'static,
    stage: &'static str,
    active: Option<Arc<AtomicBool>>,
) -> Option<Result<T, AppError>>
where
    T: Send + 'static,
{
    let mut task = OwnedTask::spawn(operation, stage, active);
    tokio::select! {
        result = task.wait() => result,
        () = deadline => None,
    }
}

#[cfg(test)]
pub(crate) use producer::capture_ack_clock_for_test;

async fn await_owned_unbounded<T>(
    operation: impl Future<Output = Result<T, AppError>> + Send + 'static,
    stage: &'static str,
) -> Option<Result<T, AppError>>
where
    T: Send + 'static,
{
    let mut task = OwnedTask::spawn(operation, stage, None);
    task.wait().await
}

#[cfg(test)]
pub(crate) async fn process_one_for_test(state: &crate::AppState) -> bool {
    upload::process_one(state, uuid::Uuid::new_v4()).await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) async fn observed_claim_for_test(
    db: &crate::db::Database,
    owner: uuid::Uuid,
) -> Result<Option<crate::db::ArchiveSpoolTask>, crate::error::AppError> {
    upload::observe_claim(db.claim_response_archive_spool_if(owner, || true)).await
}
