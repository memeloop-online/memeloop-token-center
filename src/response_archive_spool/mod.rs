//! Durable, encrypted response capture. Object storage is never on the
//! client-facing delivery path; only a short, bounded database ACK is.
mod cipher;
mod producer;
mod upload;

pub(crate) use producer::{ResponseArchiveProducer, mark_gap};
pub(crate) use upload::run;

const CHUNK_BYTES: usize = 64 * 1024;
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(test)]
mod tests;
