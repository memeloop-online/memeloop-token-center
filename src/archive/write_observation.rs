//! Logical archive API observations, not provider allocation or disk usage.
//! Calls retried by callers are counted again; SDK-internal retries are opaque.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use super::ArchiveStore;

const BACKENDS: [&str; 2] = ["primary", "configured_archive"];
const PURPOSES: [&str; 4] = ["staging", "cas", "root", "other"];
const OPERATIONS: [&str; 6] = [
    "put",
    "copy",
    "multipart_begin",
    "multipart_complete",
    "multipart_abort",
    "cleanup",
];
const OUTCOMES: [&str; 4] = [
    "success",
    "already_exists",
    "error_unknown",
    "cancelled_unknown",
];
const CELLS: usize = BACKENDS.len() * PURPOSES.len() * OPERATIONS.len() * OUTCOMES.len();

pub(super) struct WriteObservations {
    calls: [AtomicU64; CELLS],
    bytes: [AtomicU64; CELLS],
    unknown_bytes: [AtomicU64; CELLS],
}

impl Default for WriteObservations {
    fn default() -> Self {
        Self {
            calls: std::array::from_fn(|_| AtomicU64::new(0)),
            bytes: std::array::from_fn(|_| AtomicU64::new(0)),
            unknown_bytes: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

#[derive(Clone)]
pub(super) struct Context {
    metrics: Arc<WriteObservations>,
    backend: usize,
    purpose: usize,
}

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Put,
    Copy,
    MultipartBegin,
    MultipartComplete,
    MultipartAbort,
    Cleanup,
}

pub(super) struct Observation {
    context: Context,
    operation: usize,
    outcome: usize,
    bytes: Option<u64>,
}

impl Context {
    pub(super) fn start(&self, operation: Operation, bytes: Option<u64>) -> Observation {
        Observation {
            context: self.clone(),
            operation: operation as usize,
            outcome: 3,
            bytes,
        }
    }
}

impl Observation {
    pub(super) fn app_result<T>(&mut self, result: &Result<T, crate::error::AppError>) {
        self.outcome = if result.is_ok() { 0 } else { 2 };
    }

    pub(super) fn result<T>(&mut self, result: &object_store::Result<T>) {
        self.outcome = match result {
            Ok(_) => 0,
            Err(object_store::Error::AlreadyExists { .. }) => 1,
            Err(_) => 2,
        };
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        let index = (((self.context.backend * PURPOSES.len() + self.context.purpose)
            * OPERATIONS.len()
            + self.operation)
            * OUTCOMES.len())
            + self.outcome;
        let metrics = &self.context.metrics;
        metrics.calls[index].fetch_add(1, Ordering::Relaxed);
        match self.bytes {
            Some(bytes) => {
                metrics.bytes[index].fetch_add(bytes, Ordering::Relaxed);
            }
            None => {
                metrics.unknown_bytes[index].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl WriteObservations {
    pub(super) fn render(&self) -> String {
        let mut output = String::from(
            "# HELP memeloop_token_center_archive_logical_operations_total Logical archive calls, not physical storage allocation. SDK internal retries are not counted.\n# TYPE memeloop_token_center_archive_logical_operations_total counter\n# HELP memeloop_token_center_archive_logical_bytes_total Known input bytes per logical operation; never sum across operations as disk usage.\n# TYPE memeloop_token_center_archive_logical_bytes_total counter\n# HELP memeloop_token_center_archive_logical_unknown_bytes_total Logical calls whose byte length was unavailable.\n# TYPE memeloop_token_center_archive_logical_unknown_bytes_total counter\n",
        );
        for (index, count) in self.calls.iter().enumerate() {
            let calls = count.load(Ordering::Relaxed);
            if calls == 0 {
                continue;
            }
            let outcome = index % OUTCOMES.len();
            let operation = index / OUTCOMES.len() % OPERATIONS.len();
            let purpose = index / OUTCOMES.len() / OPERATIONS.len() % PURPOSES.len();
            let backend = index / OUTCOMES.len() / OPERATIONS.len() / PURPOSES.len();
            let labels = format!(
                "backend=\"{}\",purpose=\"{}\",operation=\"{}\",outcome=\"{}\"",
                BACKENDS[backend], PURPOSES[purpose], OPERATIONS[operation], OUTCOMES[outcome]
            );
            let unknown = self.unknown_bytes[index].load(Ordering::Relaxed);
            output.push_str(&format!("memeloop_token_center_archive_logical_operations_total{{{labels}}} {calls}\nmemeloop_token_center_archive_logical_unknown_bytes_total{{{labels}}} {unknown}\n"));
            // Entirely unknown lengths do not produce a misleading zero-byte series.
            if unknown < calls {
                output.push_str(&format!(
                    "memeloop_token_center_archive_logical_bytes_total{{{labels}}} {}\n",
                    self.bytes[index].load(Ordering::Relaxed)
                ));
            }
        }
        output
    }
}

impl ArchiveStore {
    /// Call only after resolve succeeds. Backend labels are configured routes,
    /// not a claim that aliases refer to different physical namespaces.
    pub(super) fn write_context(&self, location: &str) -> Context {
        let (backend, key) = if let Some(rest) = location
            .strip_prefix(super::routing::PREFIX)
            .or_else(|| location.strip_prefix(super::routing::STAGING_PREFIX))
        {
            (1, rest.split_once('/').map(|(_, key)| key).unwrap_or(rest))
        } else {
            (0, location)
        };
        let purpose = if key.starts_with("staging/") {
            0
        } else if super::path::is_any_v1_cas_location(key) {
            1
        } else if super::path::is_durable_root_location(key) {
            2
        } else {
            3
        };
        Context {
            metrics: self.write_observations.clone(),
            backend,
            purpose,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn archive_write_observations_keep_routes_outcomes_and_lengths_distinct() {
        let primary = ArchiveStore::memory_for_test();
        primary
            .put("staging/secret-request/body", Bytes::from_static(b"abc"))
            .await
            .unwrap();
        let tenant = uuid::Uuid::now_v7();
        let archive = ArchiveStore::training_memory_for_test();
        archive
            .put_tenant_content(tenant, Bytes::from_static(b"abc"))
            .await
            .unwrap();
        archive
            .clone()
            .put_tenant_content(tenant, Bytes::from_static(b"abc"))
            .await
            .unwrap();
        let metrics = archive.readiness_metrics();
        assert!(metrics.contains("backend=\"configured_archive\",purpose=\"cas\",operation=\"put\",outcome=\"success\"} 1"));
        assert!(metrics.contains("backend=\"configured_archive\",purpose=\"cas\",operation=\"put\",outcome=\"already_exists\"} 1"));
        assert!(metrics.contains("archive_logical_bytes_total{backend=\"configured_archive\",purpose=\"cas\",operation=\"put\",outcome=\"success\"} 3"));
        let context = primary.write_context("staging/secret-request/body");
        drop(context.start(Operation::Cleanup, None));
        let mut failed = context.start(Operation::Put, Some(7));
        failed.result::<()>(&Err(object_store::Error::Generic {
            store: "secret-store",
            source: Box::new(std::io::Error::other("secret-error")),
        }));
        drop(failed);
        let metrics = primary.readiness_metrics();
        assert!(metrics.contains("archive_logical_unknown_bytes_total{backend=\"primary\",purpose=\"staging\",operation=\"cleanup\",outcome=\"cancelled_unknown\"} 1"));
        assert!(metrics.contains("archive_logical_bytes_total{backend=\"primary\",purpose=\"staging\",operation=\"put\",outcome=\"error_unknown\"} 7"));
        assert!(!metrics.contains("secret-"));
        assert!(!metrics.contains(&tenant.to_string()));
    }

    #[tokio::test]
    async fn archive_write_observations_count_encoded_multipart_not_decoded_copy_bytes() {
        let archive = ArchiveStore::memory_for_test();
        let mut writer = archive
            .start_compressed_writer("staging/example/body")
            .await
            .unwrap();
        writer.write(Bytes::from(vec![b'a'; 8192])).await.unwrap();
        let staged = writer.finish_staged().await.unwrap();
        archive
            .promote_staged_text_to_cas(uuid::Uuid::now_v7(), &staged)
            .await
            .unwrap();
        let metrics = archive.readiness_metrics();
        assert!(metrics.contains("archive_logical_operations_total{backend=\"primary\",purpose=\"staging\",operation=\"multipart_complete\",outcome=\"success\"} 1"));
        // A compressed CAS promotion's decoded length must not be attributed
        // as encoded copy bytes without an existing provider size observation.
        assert!(metrics.contains("archive_logical_unknown_bytes_total{backend=\"primary\",purpose=\"cas\",operation=\"copy\",outcome=\"success\"} 1"));
    }
}
