#[cfg(feature = "h2-attempt-observer")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(feature = "h2-attempt-observer")]
static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
#[doc = "Optional metadata-only request binding. Does not retain any transport handle."]
pub struct H2AttemptObserver(Arc<Mutex<Option<Binding>>>);

#[derive(Clone)]
struct Binding {
    connection: Arc<ConnectionState>,
    stream_id: u32,
    stream_ordinal: u64,
}

struct ConnectionState {
    id: u64,
    born: Instant,
    terminal: Mutex<Option<(u8, Instant)>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc = "Local protocol-task snapshot; no peer delivery or TCP-close guarantee."]
pub struct H2AttemptSnapshot {
    #[doc = "Opaque process-local connection token."]
    pub connection_id: u64,
    #[doc = "Actual H2 stream identifier allocated by the library."]
    pub stream_id: u32,
    #[doc = "Successful allocation ordinal, including unobserved streams."]
    pub stream_ordinal: u64,
    #[doc = "Elapsed milliseconds since successful local handshake."]
    pub connection_age_ms: u64,
    #[doc = "0 running, 1 task completed Ok, 2 task completed Err, 3 task dropped."]
    pub lifecycle: u8,
    #[doc = "Terminal transition milliseconds since handshake, if known."]
    pub terminal_age_ms: Option<u64>,
}

impl H2AttemptObserver {
    #[doc = "Returns no binding before successful stream allocation or when disabled."]
    pub fn snapshot(&self) -> Option<H2AttemptSnapshot> {
        let binding = self.0.lock().ok()?.clone()?;
        let terminal = *binding.connection.terminal.lock().ok()?;
        Some(H2AttemptSnapshot {
            connection_id: binding.connection.id,
            stream_id: binding.stream_id,
            stream_ordinal: binding.stream_ordinal,
            connection_age_ms: millis(binding.connection.born.elapsed()),
            lifecycle: terminal.map_or(0, |(state, _)| state),
            terminal_age_ms: terminal
                .map(|(_, at)| millis(at.duration_since(binding.connection.born))),
        })
    }
}

fn millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(feature = "h2-attempt-observer")]
pub(crate) struct H2Connection(Arc<ConnectionState>, bool);

#[cfg(feature = "h2-attempt-observer")]
impl H2Connection {
    pub(crate) fn new() -> Self {
        Self(
            Arc::new(ConnectionState {
                id: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
                born: Instant::now(),
                terminal: Mutex::new(None),
            }),
            true,
        )
    }

    pub(crate) fn observer_state(&self) -> Self {
        Self(self.0.clone(), false)
    }

    pub(crate) fn bind(&self, observer: &H2AttemptObserver, stream_id: u32, ordinal: u64) {
        if let Ok(mut binding) = observer.0.lock() {
            *binding = Some(Binding {
                connection: self.0.clone(),
                stream_id,
                stream_ordinal: ordinal,
            });
        }
    }

    pub(crate) fn finish(&self, state: u8) {
        if let Ok(mut terminal) = self.0.terminal.lock() {
            if terminal.is_none() {
                *terminal = Some((state, Instant::now()));
            }
        }
    }
}

#[cfg(feature = "h2-attempt-observer")]
impl Drop for H2Connection {
    fn drop(&mut self) {
        if self.1 {
            self.finish(3);
        }
    }
}
