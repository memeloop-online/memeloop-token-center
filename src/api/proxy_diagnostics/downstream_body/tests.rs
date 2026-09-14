use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Writer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture(operation: impl FnOnce()) -> Vec<serde_json::Value> {
    let writer = Writer::default();
    let sink = writer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(move || sink.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, operation);
    let bytes = writer.0.lock().unwrap();
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["fields"].clone())
        .collect()
}

type FramePoll<B> = Poll<Option<Result<Frame<<B as HttpBody>::Data>, <B as HttpBody>::Error>>>;

fn poll<B: HttpBody + Unpin>(body: &mut B) -> FramePoll<B> {
    Pin::new(body).poll_frame(&mut TaskContext::from_waker(
        futures_util::task::noop_waker_ref(),
    ))
}

struct Frames(VecDeque<Frame<Bytes>>);

impl HttpBody for Frames {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
    fn is_end_stream(&self) -> bool {
        self.0.is_empty()
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(
            self.0
                .iter()
                .filter_map(Frame::data_ref)
                .map(|bytes| bytes.len() as u64)
                .sum(),
        )
    }
}

#[test]
fn final_frame_and_trailers_are_forwarded_without_needing_an_extra_eof_poll() {
    let context = Context::new();
    let events = capture(|| {
        let mut trailers = http::HeaderMap::new();
        trailers.insert(
            "x-trailer",
            http::HeaderValue::from_static("SECRET_TRAILER_CANARY"),
        );
        let source = Frames(VecDeque::from([
            Frame::data(Bytes::from_static(b"SECRET_BODY_CANARY")),
            Frame::trailers(trailers.clone()),
        ]));
        let mut body = ObservedBody::new(Body::new(source), context, 200);
        assert_eq!(body.size_hint().exact(), Some(18));
        assert!(!body.is_end_stream());
        let Poll::Ready(Some(Ok(frame))) = poll(&mut body) else {
            panic!("data frame missing")
        };
        assert_eq!(
            frame.into_data().unwrap(),
            Bytes::from_static(b"SECRET_BODY_CANARY")
        );
        assert!(!body.finished);
        let Poll::Ready(Some(Ok(frame))) = poll(&mut body) else {
            panic!("trailer frame missing")
        };
        assert_eq!(frame.into_trailers().unwrap(), trailers);
        assert!(body.is_end_stream());
        assert!(body.finished);
        drop(body);
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["request_id"], context.request_id.to_string());
    assert_eq!(events[0]["outcome"], "end_stream");
    assert_eq!(events[0]["frames"], 2);
    assert_eq!(events[0]["bytes"], 18);
    assert!(!serde_json::to_string(&events).unwrap().contains("SECRET_"));
}

#[test]
fn eos_remains_pending_until_the_original_sender_owner_releases_it() {
    let events = capture(|| {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .try_send(Ok::<_, std::io::Error>(Bytes::from_static(b"final frame")))
            .unwrap();
        let inner = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(receiver));
        let mut body = ObservedBody::new(inner, Context::new(), 200);
        assert!(matches!(poll(&mut body), Poll::Ready(Some(Ok(_)))));
        assert!(!body.finished);
        assert!(poll(&mut body).is_pending());
        assert!(!body.finished, "a queued terminal is not an observed EOF");
        drop(sender);
        assert!(matches!(poll(&mut body), Poll::Ready(None)));
        assert!(body.finished);
        drop(body);
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "end_stream");
    assert_eq!(events[0]["bytes"], 11);
}

#[test]
fn pending_drop_is_not_eof_and_does_not_retain_the_inner_owner() {
    let owner = Arc::new(());
    let events = capture(|| {
        let stream = futures_util::stream::unfold(owner.clone(), |owner| async move {
            std::future::pending::<()>().await;
            Some((Ok::<Bytes, std::io::Error>(Bytes::new()), owner))
        });
        let mut body = ObservedBody::new(Body::from_stream(stream), Context::new(), 200);
        assert!(poll(&mut body).is_pending());
        assert_eq!(Arc::strong_count(&owner), 2);
        drop(body);
        assert_eq!(Arc::strong_count(&owner), 1);
    });
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "dropped");
    assert_eq!(events[0]["polls"], 1);
    assert_eq!(events[0]["bytes"], 0);
}

#[test]
fn body_errors_and_unpolled_empty_responses_are_distinct_and_do_not_log_error_text() {
    let events = capture(|| {
        let stream = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other(
            "SECRET_ERROR_CANARY",
        ))]);
        let mut body = ObservedBody::new(Body::from_stream(stream), Context::new(), 200);
        assert!(matches!(poll(&mut body), Poll::Ready(Some(Err(_)))));
        drop(body);
        drop(ObservedBody::new(Body::empty(), Context::new(), 401));
    });
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["outcome"], "body_error");
    assert_eq!(events[1]["outcome"], "end_stream_unpolled");
    assert_eq!(events[1]["status"], 401);
    assert!(!serde_json::to_string(&events).unwrap().contains("SECRET_"));
}
