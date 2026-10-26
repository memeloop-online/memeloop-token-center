//! Deliver an HTTP/1 rejection before disposing of an unread upload.
//!
//! Hyper 1.11's h1 `poll_drain_or_close_read` tries only one body read after
//! the receiver is dropped. Closing the socket with unread TCP data can then
//! reset the peer's connection, hiding an already-written 413 from an ingress.
//! On this path, send the write FIN first, then briefly discard incoming bytes.
//! This is transport cleanup, never request admission or application buffering.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::Sleep,
};

const REJECTED_UPLOAD_LINGER: Duration = Duration::from_secs(1);
const DISCARD_BUFFER_BYTES: usize = 8 * 1024;
const MAX_READS_PER_POLL: usize = 8;

pub(super) struct RejectionIo {
    stream: TcpStream,
    rejected: Arc<AtomicBool>,
    deadline: Option<Pin<Box<Sleep>>>,
}

impl RejectionIo {
    pub(super) fn new(stream: TcpStream) -> (Self, Arc<AtomicBool>) {
        let rejected = Arc::new(AtomicBool::new(false));
        (
            Self {
                stream,
                rejected: rejected.clone(),
                deadline: None,
            },
            rejected,
        )
    }
}

impl AsyncRead for RejectionIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for RejectionIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Hyper flushes the response before shutdown. Do not wait for a body
        // byte (or send 100 Continue) before making its final response visible.
        std::task::ready!(Pin::new(&mut self.stream).poll_shutdown(cx))?;
        if !self.rejected.load(Ordering::Relaxed) {
            return Poll::Ready(Ok(()));
        }
        let deadline = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(REJECTED_UPLOAD_LINGER)));
        if deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        let mut discard = [0_u8; DISCARD_BUFFER_BYTES];
        for _ in 0..MAX_READS_PER_POLL {
            let mut buffer = ReadBuf::new(&mut discard);
            match Pin::new(&mut self.stream).poll_read(cx, &mut buffer) {
                Poll::Ready(Ok(())) if !buffer.filled().is_empty() => {}
                Poll::Ready(_) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        // A continuously writable sender must not monopolize an executor or
        // prevent the absolute deadline from being polled.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
