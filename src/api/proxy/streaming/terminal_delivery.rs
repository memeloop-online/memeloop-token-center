use axum::body::Bytes;

use crate::api::sse::ResponsesStreamingSanitizer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalEof {
    Flush,
    Complete,
    Error(&'static str),
}

/// Owns the one local EOF flush slot. `streaming.rs` only orchestrates the
/// upstream and downstream state machines; this type decides whether EOF can
/// release a held success terminal or must fail closed.
#[derive(Default)]
pub(super) enum ResponsesTerminalDelivery {
    #[default]
    Reading,
    FlushPending(Bytes),
    Finished,
}

impl ResponsesTerminalDelivery {
    pub(super) fn take_pending(&mut self) -> Option<Bytes> {
        match std::mem::replace(self, Self::Finished) {
            Self::FlushPending(bytes) => Some(bytes),
            state => {
                *self = state;
                None
            }
        }
    }

    pub(super) fn upstream_poll_allowed(&self) -> bool {
        matches!(self, Self::Reading)
    }

    pub(super) fn finish_at_eof(
        &mut self,
        sanitizer: Option<&mut ResponsesStreamingSanitizer>,
    ) -> TerminalEof {
        if !matches!(self, Self::Reading) {
            return TerminalEof::Complete;
        }
        let Some(sanitizer) = sanitizer else {
            *self = Self::Finished;
            return TerminalEof::Complete;
        };
        match sanitizer.finish() {
            Ok(bytes) if !bytes.is_empty() => {
                *self = Self::FlushPending(bytes);
                TerminalEof::Flush
            }
            Ok(_) => {
                *self = Self::Finished;
                TerminalEof::Complete
            }
            Err(error_code) => {
                *self = Self::Finished;
                TerminalEof::Error(error_code)
            }
        }
    }
}

#[cfg(test)]
#[path = "terminal_delivery/tests.rs"]
mod tests;
