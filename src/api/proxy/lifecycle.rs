pub(super) struct AbortTaskOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortTaskOnDrop<T> {
    pub(super) fn new(task: tokio::task::JoinHandle<T>) -> Self {
        Self(Some(task))
    }

    pub(super) fn abort(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.abort();
    }
}

pub(super) async fn run_bounded_proxy_lifecycle<F>(
    deadline: tokio::time::Instant,
    lifecycle: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, lifecycle).await
}
