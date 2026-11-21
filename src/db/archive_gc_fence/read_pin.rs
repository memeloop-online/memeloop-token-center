//! A read pin owns no body bytes. Drop schedules an explicit release only after
//! the protected future/stream is no longer capable of another object read.
use super::*;
use futures_util::Stream;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

pub(crate) struct ArchiveGcReadPin {
    database: Database,
    pins: Option<ArchiveGcPins>,
}

impl Drop for ArchiveGcReadPin {
    fn drop(&mut self) {
        let Some(pins) = self.pins.take() else {
            return;
        };
        // No runtime means no release: durable coordination remains fail-closed
        // until the existing owner recovery proves the reader is gone.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let database = self.database.clone();
            runtime.spawn(async move {
                if let Err(error) = database.release_archive_gc_pins(&pins).await {
                    tracing::warn!(
                        error_category = error.diagnostic_category(),
                        "completed archive reader retains durable pin for reconciliation"
                    );
                }
            });
        }
    }
}

impl ArchiveGcReadPin {
    pub(crate) fn protect_stream<S: Stream + Send + 'static>(
        self,
        source: S,
    ) -> impl Stream<Item = S::Item> + Send {
        PinnedArchiveStream {
            source: Some(Box::pin(source)),
            guard: Some(self),
        }
    }
}

struct PinnedArchiveStream<S> {
    // Drop order matters: cancel the object stream before releasing its pin.
    source: Option<Pin<Box<S>>>,
    guard: Option<ArchiveGcReadPin>,
}
impl<S: Stream> Stream for PinnedArchiveStream<S> {
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(source) = self.source.as_mut() else {
            return Poll::Ready(None);
        };
        let result = source.as_mut().poll_next(context);
        if matches!(result, Poll::Ready(None)) {
            // A stream is allowed to be non-fused. End and drop it permanently
            // before releasing the durable read pin.
            self.source.take();
            self.guard.take();
        }
        result
    }
}

impl Database {
    pub(crate) async fn pin_archive_reader(
        &self,
        tenant: Uuid,
        request: Uuid,
    ) -> Result<ArchiveGcReadPin, AppError> {
        let pins = self
            .acquire_archive_gc_pins(
                vec![ArchiveGcScope::for_tenant(tenant)?],
                Uuid::now_v7(),
                "archive-reader",
                request,
                ArchiveGcPinPurpose::Reader,
            )
            .await?
            .ok_or_else(|| {
                AppError::Conflict("archive maintenance is in progress; retry this read".into())
            })?;
        Ok(ArchiveGcReadPin {
            database: self.clone(),
            pins: Some(pins),
        })
    }
}
