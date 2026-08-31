use std::{io, time::Duration};

use super::*;
use crate::metrics::{ProfileKind, allocator_runtime_metrics};
#[cfg(target_os = "linux")]
use pprof::protos::Message;

const DEFAULT_PROFILE_SECONDS: u8 = 10;
const MAX_PROFILE_SECONDS: u8 = 30;
const MAX_CPU_PROFILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_HEAP_PROFILE_BYTES: usize = 64 * 1024 * 1024;
const PROFILE_FINISH_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
pub(super) struct ProfileQuery {
    seconds: Option<u8>,
}

impl ProfileQuery {
    fn duration(&self) -> Result<Duration, AppError> {
        let seconds = self.seconds.unwrap_or(DEFAULT_PROFILE_SECONDS);
        if !(1..=MAX_PROFILE_SECONDS).contains(&seconds) {
            return Err(AppError::BadRequest(format!(
                "seconds must be between 1 and {MAX_PROFILE_SECONDS}"
            )));
        }
        Ok(Duration::from_secs(u64::from(seconds)))
    }
}

pub(super) async fn runtime_diagnostics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    require_service(&headers, &state, "metrics:read").await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "process": state.metrics.process_runtime_metrics(),
            "allocator": allocator_runtime_metrics(),
            "profiling": {
                "enabled": true,
                "cpu_max_seconds": MAX_PROFILE_SECONDS,
                "heap_max_seconds": MAX_PROFILE_SECONDS,
            }
        })),
    )
        .into_response())
}

#[cfg(target_os = "linux")]
pub(super) async fn cpu_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProfileQuery>,
) -> Result<Response, AppError> {
    require_service(&headers, &state, "metrics:read").await?;
    let duration = query.duration()?;
    let profile_guard = state
        .metrics
        .try_begin_profile(ProfileKind::Cpu)
        .ok_or_else(|| AppError::Conflict("a CPU profile is already running".to_owned()))?;
    let task = tokio::task::spawn_blocking(move || {
        // The singleflight guard stays in the blocking task even if the HTTP
        // request is cancelled or the outer timeout expires.
        let _profile_guard = profile_guard;
        capture_cpu_profile(duration)
    });
    let bytes = tokio::time::timeout(duration + PROFILE_FINISH_GRACE, task)
        .await
        .map_err(|_| AppError::Internal)?
        .map_err(|_| AppError::Internal)??;
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.google.protobuf"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=memeloop-token-center-cpu-profile.pb",
            ),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(target_os = "linux")]
fn capture_cpu_profile(duration: Duration) -> Result<Vec<u8>, AppError> {
    let profiler = pprof::ProfilerGuardBuilder::default()
        .frequency(99)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .map_err(|_| AppError::Internal)?;
    std::thread::sleep(duration);
    let report = profiler.report().build().map_err(|_| AppError::Internal)?;
    let profile = report.pprof().map_err(|_| AppError::Internal)?;
    let mut output = Vec::with_capacity(profile.encoded_len().min(MAX_CPU_PROFILE_BYTES));
    profile
        .encode(&mut output)
        .map_err(|_| AppError::Internal)?;
    if output.is_empty() || output.len() > MAX_CPU_PROFILE_BYTES {
        return Err(AppError::Internal);
    }
    Ok(output)
}

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
pub(super) async fn heap_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProfileQuery>,
) -> Result<Response, AppError> {
    require_service(&headers, &state, "metrics:read").await?;
    let duration = query.duration()?;
    let profile_guard = state
        .metrics
        .try_begin_profile(ProfileKind::Heap)
        .ok_or_else(|| AppError::Conflict("a heap profile is already running".to_owned()))?;
    let task = tokio::task::spawn_blocking(move || {
        let _profile_guard = profile_guard;
        capture_heap_profile(duration)
    });
    let bytes = tokio::time::timeout(duration + PROFILE_FINISH_GRACE, task)
        .await
        .map_err(|_| AppError::Internal)?
        .map_err(|_| AppError::Internal)??;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=memeloop-token-center.heap",
            ),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
fn capture_heap_profile(duration: Duration) -> Result<Vec<u8>, AppError> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, path::PathBuf};
    if crate::jemalloc_control::read_bool(b"opt.prof\0") != Some(true) {
        return Err(AppError::Internal);
    }
    let active = JemallocProfileActivation::begin()?;
    std::thread::sleep(duration);
    let path = PathBuf::from("/tmp").join(format!("memeloop-token-center-{}.heap", Uuid::now_v7()));
    let temporary = TemporaryProfile(path);
    let path = CString::new(temporary.0.as_os_str().as_bytes()).map_err(|_| AppError::Internal)?;
    // SAFETY: `prof.dump` expects a pointer to a NUL-terminated path and
    // jemalloc consumes it synchronously before CString is dropped.
    crate::jemalloc_control::write_pointer(b"prof.dump\0", path.as_ptr())
        .map_err(|_| AppError::Internal)?;
    drop(active);
    let metadata = std::fs::metadata(&temporary.0).map_err(|_| AppError::Internal)?;
    if metadata.len() > MAX_HEAP_PROFILE_BYTES as u64 {
        return Err(AppError::Internal);
    }
    std::fs::read(&temporary.0).map_err(|_| AppError::Internal)
}

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
struct JemallocProfileActivation {
    previous: bool,
}

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
impl JemallocProfileActivation {
    fn begin() -> Result<Self, AppError> {
        // SAFETY: `prof.active` has the documented jemalloc `bool` type.
        let previous = crate::jemalloc_control::update_bool(b"prof.active\0", true)
            .map_err(|_| AppError::Internal)?;
        Ok(Self { previous })
    }
}

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
impl Drop for JemallocProfileActivation {
    fn drop(&mut self) {
        // SAFETY: `prof.active` has the documented jemalloc `bool` type.
        let result = crate::jemalloc_control::update_bool(b"prof.active\0", self.previous);
        if result.is_err() {
            tracing::error!("failed to restore jemalloc profiling state");
        }
    }
}

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
struct TemporaryProfile(std::path::PathBuf);

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
impl Drop for TemporaryProfile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => tracing::warn!("failed to remove a temporary heap profile"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_strictly_bounded() {
        assert_eq!(
            ProfileQuery { seconds: None }.duration().unwrap(),
            Duration::from_secs(10)
        );
        assert!(ProfileQuery { seconds: Some(0) }.duration().is_err());
        assert!(
            ProfileQuery {
                seconds: Some(MAX_PROFILE_SECONDS + 1)
            }
            .duration()
            .is_err()
        );
    }
}
