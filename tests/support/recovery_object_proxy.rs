//! Transparent S3 fault proxy for recovery scenarios; object paths stay in memory.
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::Response,
    routing::any,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;

#[derive(Default)]
pub(crate) struct ObjectReadGate {
    entered: Notify,
    released: AtomicBool,
    release: Notify,
}
impl ObjectReadGate {
    pub(crate) async fn entered(&self) {
        self.entered.notified().await;
    }
    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
        self.release.notify_one();
    }
}
#[derive(Default)]
struct Faults {
    gets: HashMap<String, Arc<ObjectReadGate>>,
    puts: HashSet<String>,
    put_prefixes: HashSet<String>,
    put_scopes: HashSet<(String, String)>,
}
struct ProxyState {
    upstream: String,
    client: reqwest::Client,
    faults: Mutex<Faults>,
    failed_puts: AtomicUsize,
}
pub(crate) struct RecoveryObjectProxy {
    endpoint: String,
    state: Arc<ProxyState>,
    task: tokio::task::JoinHandle<()>,
}
impl RecoveryObjectProxy {
    pub(crate) async fn start(upstream: &str) -> Self {
        let state = Arc::new(ProxyState {
            upstream: upstream.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            faults: Mutex::new(Faults::default()),
            failed_puts: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .fallback(any(forward))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            endpoint,
            state,
            task,
        }
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(crate) fn gate_get(&self, path: &str) -> Arc<ObjectReadGate> {
        assert!(path.starts_with('/'));
        let gate = Arc::new(ObjectReadGate::default());
        assert!(
            self.state
                .faults
                .lock()
                .unwrap()
                .gets
                .insert(path.to_owned(), gate.clone())
                .is_none()
        );
        gate
    }
    pub(crate) fn fail_put(&self, path: &str, enabled: bool) {
        assert!(path.starts_with('/'));
        let mut faults = self.state.faults.lock().unwrap();
        if enabled {
            faults.puts.insert(path.to_owned());
        } else {
            faults.puts.remove(path);
        }
    }
    pub(crate) fn fail_put_prefix(&self, prefix: &str, enabled: bool) {
        assert!(prefix.starts_with('/') && prefix.len() > 1);
        let mut faults = self.state.faults.lock().unwrap();
        if enabled {
            faults.put_prefixes.insert(prefix.to_owned());
        } else {
            faults.put_prefixes.remove(prefix);
        }
    }
    pub(crate) fn fail_put_scope(&self, prefix: &str, suffix: &str, enabled: bool) {
        assert!(prefix.starts_with('/') && prefix.len() > 1 && !suffix.is_empty());
        let mut faults = self.state.faults.lock().unwrap();
        let scope = (prefix.to_owned(), suffix.to_owned());
        if enabled {
            faults.put_scopes.insert(scope);
        } else {
            faults.put_scopes.remove(&scope);
        }
    }
    pub(crate) fn failed_puts(&self) -> usize {
        self.state.failed_puts.load(Ordering::SeqCst)
    }
}
impl Drop for RecoveryObjectProxy {
    fn drop(&mut self) {
        for gate in self.state.faults.lock().unwrap().gets.values() {
            gate.release();
        }
        self.task.abort();
    }
}
async fn forward(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let path = request.uri().path();
    let (gate, fail_put) = {
        let faults = state.faults.lock().unwrap();
        (
            if request.method() == axum::http::Method::GET {
                faults.gets.get(path).cloned()
            } else {
                None
            },
            request.method() == axum::http::Method::PUT
                && (faults.puts.contains(path)
                    || faults
                        .put_prefixes
                        .iter()
                        .any(|prefix| path.starts_with(prefix))
                    || faults.put_scopes.iter().any(|(prefix, suffix)| {
                        path.starts_with(prefix) && path.ends_with(suffix)
                    })),
        )
    };
    if let Some(gate) = gate {
        // Register the release waiter before announcing entry to avoid races.
        let release = gate.release.notified();
        tokio::pin!(release);
        release.as_mut().enable();
        gate.entered.notify_one();
        if !gate.released.load(Ordering::SeqCst) {
            release.await;
        }
    }
    if fail_put {
        state.failed_puts.fetch_add(1, Ordering::SeqCst);
        return Response::builder()
            .status(503)
            .body(Body::from("temporarily unavailable"))
            .unwrap();
    }
    let (parts, body) = request.into_parts();
    let url = format!(
        "{}{}",
        state.upstream,
        parts
            .uri
            .path_and_query()
            .map(|part| part.as_str())
            .unwrap_or("/")
    );
    let body = reqwest::Body::wrap_stream(body.into_data_stream());
    // Preserve Host and signed headers as received; query and payload are unchanged.
    let response = match state
        .client
        .request(parts.method, url)
        .headers(parts.headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Response::builder().status(502).body(Body::empty()).unwrap(),
    };
    let mut output = Response::builder().status(response.status());
    *output.headers_mut().unwrap() = response.headers().clone();
    output
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap()
}
