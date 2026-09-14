//! Observe the HTTP consumer, not producer queueing or a TCP acknowledgement.
use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Instant,
};

use axum::{
    body::{Body, Bytes},
    response::Response,
};
use hyper::body::{Body as HttpBody, Frame, SizeHint};

use super::Context;

pub(super) fn observe_response(response: Response, context: Context) -> Response {
    let (parts, body) = response.into_parts();
    let observed = ObservedBody::new(body, context, parts.status.as_u16());
    Response::from_parts(parts, Body::new(observed))
}

struct ObservedBody {
    inner: Body,
    context: Context,
    status: u16,
    started: Instant,
    polls: u64,
    frames: u64,
    bytes: u64,
    first_poll_ms: Option<i64>,
    last_poll_ms: Option<i64>,
    first_data_ms: Option<i64>,
    last_data_ms: Option<i64>,
    finished: bool,
}

impl ObservedBody {
    fn new(inner: Body, context: Context, status: u16) -> Self {
        Self {
            inner,
            context,
            status,
            started: Instant::now(),
            polls: 0,
            frames: 0,
            bytes: 0,
            first_poll_ms: None,
            last_poll_ms: None,
            first_data_ms: None,
            last_data_ms: None,
            finished: false,
        }
    }

    fn finish(&mut self, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        tracing::info!(
            request_id = %self.context.request_id,
            phase = "gateway_downstream_body",
            outcome,
            status = self.status,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            request_elapsed_ms = self.context.elapsed_millis_at(Instant::now()),
            polls = self.polls,
            frames = self.frames,
            bytes = self.bytes,
            first_poll_ms = self.first_poll_ms,
            last_poll_ms = self.last_poll_ms,
            first_data_ms = self.first_data_ms,
            last_data_ms = self.last_data_ms,
            "downstream HTTP body consumer observation; not a network acknowledgement"
        );
    }
}

impl HttpBody for ObservedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.polls = self.polls.saturating_add(1);
        let now = self.context.elapsed_millis_at(Instant::now());
        self.first_poll_ms.get_or_insert(now);
        self.last_poll_ms = Some(now);
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                self.frames = self.frames.saturating_add(1);
                if let Some(data) = frame.data_ref() {
                    self.bytes = self.bytes.saturating_add(data.len() as u64);
                    if !data.is_empty() {
                        self.first_data_ms.get_or_insert(now);
                        self.last_data_ms = Some(now);
                    }
                }
                // Hyper need not poll again after a body's final frame.
                if self.inner.is_end_stream() {
                    self.finish("end_stream");
                }
            }
            Poll::Ready(Some(Err(_))) => self.finish("body_error"),
            Poll::Ready(None) => self.finish("end_stream"),
            Poll::Pending => {}
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        // An empty/HEAD response may never be polled. Do not call this a
        // disconnect, and do not claim any application bytes were consumed.
        self.finish(if self.inner.is_end_stream() {
            "end_stream_unpolled"
        } else {
            "dropped"
        });
    }
}

#[cfg(test)]
mod tests;
