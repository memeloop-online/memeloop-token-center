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
/// One run-owned physical object, bound before its successful PUT ACK escapes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExactDeleteBinding {
    pub(crate) backend: String,
    pub(crate) host: String,
    pub(crate) path: String,
}
#[derive(Default)]
pub(crate) struct ExactCarrierDeleteFault {
    bucket: String,
    binding: Mutex<Option<ExactDeleteBinding>>,
    ambiguous: AtomicBool,
    enabled: AtomicBool,
    rejected: AtomicUsize,
}
impl ExactCarrierDeleteFault {
    fn observes_response_carrier(&self, path: &str) -> bool {
        let fields: Vec<_> = path.trim_start_matches('/').split('/').collect();
        fields.len() == 7
            && fields[0] == self.bucket
            && fields[1] == "staging"
            && fields[2] == "proxy"
            && fields[4] == "response"
            && [fields[3], fields[5]]
                .into_iter()
                .all(|id| uuid::Uuid::parse_str(id).is_ok())
            && fields[6]
                .strip_prefix("carrier-")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    }
    fn bind_after_put(&self, backend: &str, host: &str, path: &str) {
        if !self.observes_response_carrier(path) {
            return;
        }
        let candidate = ExactDeleteBinding {
            backend: backend.into(),
            host: host.into(),
            path: path.into(),
        };
        let mut bound = self.binding.lock().unwrap();
        match bound.as_ref() {
            None => *bound = Some(candidate),
            Some(existing) if existing == &candidate => {}
            Some(_) => {
                self.ambiguous.store(true, Ordering::SeqCst);
            }
        }
    }
    fn rejects(&self, backend: &str, host: &str, path: &str, query: Option<&str>) -> bool {
        self.enabled.load(Ordering::SeqCst)
            && query.is_none()
            && self.binding.lock().unwrap().as_ref().is_some_and(|bound| {
                bound.backend == backend && bound.host == host && bound.path == path
            })
    }
    pub(crate) fn binding(&self) -> ExactDeleteBinding {
        assert!(
            !self.ambiguous.load(Ordering::SeqCst),
            "multiple response carrier objects in exclusive run"
        );
        self.binding
            .lock()
            .unwrap()
            .clone()
            .expect("real PUT ACK must bind the object")
    }
    pub(crate) fn rejected(&self) -> usize {
        self.rejected.load(Ordering::SeqCst)
    }
    pub(crate) fn release(&self) {
        self.enabled.store(false, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Faults {
    exact_carrier_delete: Option<Arc<ExactCarrierDeleteFault>>,
    gets: HashMap<String, Arc<ObjectReadGate>>,
    response_get: Option<(String, Arc<ObjectReadGate>)>,
    part_gate: Option<Arc<ObjectReadGate>>,
    body_put_gate: Option<Arc<ObjectReadGate>>,
    abort_gate: Option<Arc<ObjectReadGate>>,
    puts: HashSet<String>,
    put_prefixes: HashSet<String>,
    put_scopes: HashSet<(String, String)>,
}
struct ProxyState {
    upstream: String,
    client: reqwest::Client,
    faults: Mutex<Faults>,
    failed_puts: AtomicUsize,
    counts: Mutex<HashMap<String, usize>>,
    timings: Mutex<HashMap<String, Vec<u64>>>,
    lose_put_ack: AtomicBool,
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
            counts: Mutex::default(),
            timings: Mutex::default(),
            lose_put_ack: AtomicBool::new(false),
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
    pub(crate) fn fail_one_response_carrier_delete_after_put(
        &self,
        bucket: &str,
    ) -> Arc<ExactCarrierDeleteFault> {
        assert!(!bucket.is_empty() && !bucket.contains('/'));
        let fault = Arc::new(ExactCarrierDeleteFault {
            bucket: bucket.into(),
            enabled: AtomicBool::new(true),
            ..Default::default()
        });
        assert!(
            self.state
                .faults
                .lock()
                .unwrap()
                .exact_carrier_delete
                .replace(fault.clone())
                .is_none()
        );
        fault
    }
    pub(crate) fn counts(&self) -> HashMap<String, usize> {
        self.state.counts.lock().unwrap().clone()
    }
    pub(crate) fn timings(&self) -> HashMap<String, Vec<u64>> {
        self.state.timings.lock().unwrap().clone()
    }
    pub(crate) fn lose_body_put_ack(&self, enabled: bool) {
        self.state.lose_put_ack.store(enabled, Ordering::SeqCst);
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub(crate) fn gate_body_put(&self) -> Arc<ObjectReadGate> {
        let gate = Arc::new(ObjectReadGate::default());
        self.state.faults.lock().unwrap().body_put_gate = Some(gate.clone());
        gate
    }
    pub(crate) fn gate_multipart_part(&self) -> Arc<ObjectReadGate> {
        let gate = Arc::new(ObjectReadGate::default());
        self.state.faults.lock().unwrap().part_gate = Some(gate.clone());
        gate
    }
    pub(crate) fn gate_multipart_abort(&self) -> Arc<ObjectReadGate> {
        let gate = Arc::new(ObjectReadGate::default());
        self.state.faults.lock().unwrap().abort_gate = Some(gate.clone());
        gate
    }
    pub(crate) fn gate_get_response_prefix(&self, prefix: &str) -> Arc<ObjectReadGate> {
        let gate = Arc::new(ObjectReadGate::default());
        self.state.faults.lock().unwrap().response_get = Some((prefix.to_owned(), gate.clone()));
        gate
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
    pub(crate) fn clear_faults(&self) {
        let mut faults = self.state.faults.lock().unwrap();
        if let Some(fault) = faults.exact_carrier_delete.take() {
            fault.release();
        }
        faults.puts.clear();
        faults.put_prefixes.clear();
        faults.put_scopes.clear();
        for gate in faults.gets.values() {
            gate.release();
        }
        faults.gets.clear();
        for gate in [
            faults.part_gate.take(),
            faults.abort_gate.take(),
            faults.body_put_gate.take(),
        ]
        .into_iter()
        .flatten()
        {
            gate.release();
        }
    }
    pub(crate) fn failed_puts(&self) -> usize {
        self.state.failed_puts.load(Ordering::SeqCst)
    }
}
impl Drop for RecoveryObjectProxy {
    fn drop(&mut self) {
        self.clear_faults();
        self.task.abort();
    }
}
async fn forward(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let began = std::time::Instant::now();
    let exact_fault = state.faults.lock().unwrap().exact_carrier_delete.clone();
    let original_path = request.uri().path().to_owned();
    let original_host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let original_method = request.method().clone();
    if original_method == axum::http::Method::DELETE
        && exact_fault.as_ref().is_some_and(|fault| {
            fault.rejects(
                &state.upstream,
                &original_host,
                &original_path,
                request.uri().query(),
            )
        })
    {
        exact_fault
            .as_ref()
            .unwrap()
            .rejected
            .fetch_add(1, Ordering::SeqCst);
        return Response::builder()
            .status(503)
            .body(Body::from("exact synthetic cleanup unavailable"))
            .unwrap();
    }
    let query = request.uri().query().unwrap_or("");
    let receipt = request.uri().path().contains("carrier-owners/")
        || request.uri().path().contains("carrier-releases/");
    let class = if receipt {
        "receipt"
    } else if request.method() == axum::http::Method::POST && query.contains("uploads") {
        "multipart_create"
    } else if request.method() == axum::http::Method::POST && query.contains("uploadId") {
        "multipart_complete"
    } else if request.method() == axum::http::Method::PUT && query.contains("uploadId") {
        "multipart_part"
    } else if request.method() == axum::http::Method::PUT {
        "body_put"
    } else if request.method() == axum::http::Method::GET
        && request.headers().contains_key(axum::http::header::RANGE)
    {
        "range_get"
    } else {
        "other"
    };
    *state
        .counts
        .lock()
        .unwrap()
        .entry(class.into())
        .or_default() += 1;
    let path = request.uri().path();
    let (gate, fail_put) = {
        let faults = state.faults.lock().unwrap();
        (
            if request.method() == axum::http::Method::GET {
                faults.gets.get(path).cloned()
            } else if request.method() == axum::http::Method::PUT && query.contains("uploadId") {
                faults.part_gate.clone()
            } else if request.method() == axum::http::Method::DELETE && query.contains("uploadId") {
                faults.abort_gate.clone()
            } else if class == "body_put" {
                faults.body_put_gate.clone()
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
    let response_gate = {
        let faults = state.faults.lock().unwrap();
        faults
            .response_get
            .as_ref()
            .filter(|(prefix, _)| {
                request.method() == axum::http::Method::GET
                    && request.uri().path().starts_with(prefix)
            })
            .map(|(_, gate)| {
                assert_eq!(
                    request.headers().get(axum::http::header::RANGE).unwrap(),
                    "bytes=0-12"
                );
                gate.clone()
            })
    };
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
    if original_method == axum::http::Method::PUT
        && response.status().is_success()
        && let Some(fault) = &exact_fault
    {
        // This runs before forwarding the accepted part/body ACK to publisher.
        // The physical endpoint + signed Host + bucket/object path stay exact.
        fault.bind_after_put(&state.upstream, &original_host, &original_path);
    }
    state
        .timings
        .lock()
        .unwrap()
        .entry(class.into())
        .or_default()
        .push(began.elapsed().as_micros().min(u64::MAX as u128) as u64);
    if class == "body_put"
        && state.lose_put_ack.load(Ordering::SeqCst)
        && response.status().is_success()
    {
        // Upstream has persisted the bytes, but the caller receives no ACK.
        return Response::builder().status(503).body(Body::empty()).unwrap();
    }
    if let Some(gate) = response_gate {
        assert!(response.status().is_success());
        let release = gate.release.notified();
        tokio::pin!(release);
        release.as_mut().enable();
        gate.entered.notify_one();
        if !gate.released.load(Ordering::SeqCst) {
            release.await;
        }
    }
    let mut output = Response::builder().status(response.status());
    *output.headers_mut().unwrap() = response.headers().clone();
    output
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap()
}

#[cfg(test)]
mod exact_delete_tests {
    use super::*;

    #[test]
    fn exact_carrier_delete_fault_keeps_backend_host_query_and_neighbor_boundaries() {
        let fault = ExactCarrierDeleteFault {
            bucket: "isolated".into(),
            enabled: AtomicBool::new(true),
            ..Default::default()
        };
        let owner = uuid::Uuid::now_v7();
        let attempt = uuid::Uuid::now_v7();
        let path = format!(
            "/isolated/staging/proxy/{owner}/response/{attempt}/carrier-{}",
            uuid::Uuid::now_v7()
        );
        assert!(!fault.rejects("backend", "signed-host", &path, None));
        fault.bind_after_put("backend", "signed-host", &path);
        assert_eq!(fault.binding().path, path);
        assert!(fault.rejects("backend", "signed-host", &path, None));
        for query in ["uploadId=abc", "versionId=abc", "delete", ""] {
            assert!(!fault.rejects("backend", "signed-host", &path, Some(query)));
        }
        assert!(!fault.rejects("other-backend", "signed-host", &path, None));
        assert!(!fault.rejects("backend", "other-host", &path, None));
        assert!(!fault.rejects("backend", "signed-host", &format!("{path}-neighbor"), None));
        assert!(!fault.observes_response_carrier(&path.replace("/isolated/", "/other-bucket/")));
        assert!(!fault.observes_response_carrier("/isolated/carrier-releases/v1/receipt.json"));
        fault.bind_after_put("backend", "signed-host", &path);
        assert!(!fault.ambiguous.load(Ordering::SeqCst));
        let second = format!(
            "/isolated/staging/proxy/{owner}/response/{attempt}/carrier-{}",
            uuid::Uuid::now_v7()
        );
        fault.bind_after_put("backend", "signed-host", &second);
        assert!(
            fault.ambiguous.load(Ordering::SeqCst),
            "different actual object makes the run unprovable"
        );
        assert!(!fault.rejects("backend", "signed-host", &second, None));
        fault.release();
        assert!(!fault.rejects("backend", "signed-host", &path, None));
    }
}
