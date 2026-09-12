//! Durable, encrypted response capture. Object storage is never on the
//! client-facing delivery path; only a short, bounded database ACK is.
mod cipher;
mod producer;
mod upload;

#[cfg(test)]
pub(crate) use producer::fail_next_append_for_test;
pub(crate) use producer::{ResponseArchiveProducer, mark_gap};
pub(crate) use upload::run;

const CHUNK_BYTES: usize = 64 * 1024;
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

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
