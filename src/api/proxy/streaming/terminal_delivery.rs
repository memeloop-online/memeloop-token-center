use axum::body::Bytes;

use crate::api::sse::ResponsesStreamingSanitizer;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum TerminalEof {
    Flush,
    Complete,
    Error(&'static str),
}

/// Owns the one local EOF flush slot. `streaming.rs` only orchestrates the
/// upstream and downstream state machines; this type decides whether EOF can
/// release a held success terminal or must fail closed.
#[derive(Default)]
pub(super) struct ResponsesTerminalDelivery {
    pending: Option<Bytes>,
}

impl ResponsesTerminalDelivery {
    pub(super) fn take_pending(&mut self) -> Option<Bytes> {
        self.pending.take()
    }

    pub(super) fn finish_at_eof(
        &mut self,
        sanitizer: Option<&mut ResponsesStreamingSanitizer>,
    ) -> TerminalEof {
        let Some(sanitizer) = sanitizer else {
            return TerminalEof::Complete;
        };
        match sanitizer.finish() {
            Ok(bytes) if !bytes.is_empty() => {
                self.pending = Some(bytes);
                TerminalEof::Flush
            }
            Ok(_) => TerminalEof::Complete,
            Err(error_code) => TerminalEof::Error(error_code),
        }
    }
}

#[cfg(test)]
#[path = "terminal_delivery/tests.rs"]
mod tests;
