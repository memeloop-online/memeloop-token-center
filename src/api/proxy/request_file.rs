use super::*;
use std::io::Read;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) async fn parse(
    file: std::fs::File,
    memory: Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
) -> Result<Value, AppError> {
    struct Cancel(Arc<AtomicBool>);
    impl Drop for Cancel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct Reader {
        file: std::fs::File,
        cancel: Arc<AtomicBool>,
    }
    impl Read for Reader {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if self.cancel.load(Ordering::Acquire) {
                return Err(std::io::Error::other("request parse cancelled"));
            }
            self.file.read(output)
        }
    }
    let cancel = Cancel(Arc::new(AtomicBool::new(false)));
    let reader = Reader {
        file,
        cancel: cancel.0.clone(),
    };
    let result = tokio::task::spawn_blocking(move || {
        // Keep the real allocation charged until a cancelled worker stops.
        let _memory = memory;
        crate::api::sse::parse_unique_json_reader(std::io::BufReader::with_capacity(
            64 * 1024,
            reader,
        ))
    })
    .await
    .map_err(|_| AppError::internal("request_json_parse_worker"))?;
    result.map_err(|_| AppError::BadRequest("request body must be valid JSON".into()))
}
