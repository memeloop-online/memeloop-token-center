use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use super::{HttpPurpose, HttpSettingsDocument};

#[derive(Default)]
pub struct ClientOptions<'a> {
    pub purpose: Option<HttpPurpose>,
    pub proxy_url: Option<&'a str>,
    pub proxy_auth: Option<(&'a str, &'a str)>,
    pub pins: &'a [(&'a str, &'a [SocketAddr])],
    pub no_retry: bool,
}

#[derive(Eq, PartialEq)]
struct ClientKey {
    purpose: HttpPurpose,
    proxy_url: Option<String>,
    proxy_auth: Option<(String, String)>,
    pins: Vec<(String, Vec<SocketAddr>)>,
    no_retry: bool,
}

pub(crate) struct HttpSnapshot {
    pub(crate) document: HttpSettingsDocument,
    clients: Mutex<VecDeque<(ClientKey, reqwest::Client)>>,
}

impl HttpSnapshot {
    fn new(document: HttpSettingsDocument) -> Self {
        Self {
            document,
            clients: Mutex::default(),
        }
    }

    pub(crate) fn builder(&self, purpose: HttpPurpose) -> reqwest::ClientBuilder {
        crate::simple_http::install_ring_provider();
        let effective = self.document.settings.effective(purpose);
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(
                effective
                    .pool_idle_timeout_ms
                    .value
                    .map(Duration::from_millis),
            )
            .pool_max_idle_per_host(effective.pool_max_idle_per_host.value)
            .tcp_keepalive(None::<Duration>)
            .tcp_keepalive_interval(None::<Duration>)
            .tcp_keepalive_retries(None::<u32>);
        if let Some(timeout) = effective.connect_timeout_ms.value {
            builder = builder.connect_timeout(Duration::from_millis(timeout));
        }
        if let Some(timeout) = effective.read_timeout_ms.value {
            builder = builder.read_timeout(Duration::from_millis(timeout));
        }
        if let Some(timeout) = effective.request_timeout_ms.value {
            builder = builder.timeout(Duration::from_millis(timeout));
        }
        builder
    }

    fn client(
        &self,
        default_purpose: HttpPurpose,
        options: ClientOptions<'_>,
    ) -> Result<reqwest::Client, reqwest::Error> {
        let purpose = options.purpose.unwrap_or(default_purpose);
        let mut pins: Vec<_> = options
            .pins
            .iter()
            .map(|(host, addresses)| {
                let mut addresses = addresses.to_vec();
                addresses.sort_unstable();
                addresses.dedup();
                ((*host).to_owned(), addresses)
            })
            .collect();
        pins.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let key = ClientKey {
            purpose,
            proxy_url: options.proxy_url.map(str::to_owned),
            proxy_auth: options
                .proxy_auth
                .map(|(user, password)| (user.to_owned(), password.to_owned())),
            pins,
            no_retry: options.no_retry,
        };
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(position) = clients.iter().position(|(existing, _)| existing == &key) {
            let entry = clients.remove(position).expect("client cache position");
            let client = entry.1.clone();
            clients.push_back(entry);
            return Ok(client);
        }
        let mut builder = self.builder(purpose);
        if options.no_retry {
            builder = builder.retry(reqwest::retry::never());
        }
        if let Some(url) = options.proxy_url {
            let mut proxy = reqwest::Proxy::all(url)?;
            if let Some((username, password)) = options.proxy_auth {
                proxy = proxy.basic_auth(username, password);
            }
            builder = builder.proxy(proxy);
        }
        for (host, addresses) in &key.pins {
            builder = builder.resolve_to_addrs(host, addresses);
        }
        let client = builder.build()?;
        if clients.len() >= 256 {
            clients.pop_front();
        }
        clients.push_back((key, client.clone()));
        Ok(client)
    }
}

pub struct HttpClientFactory {
    current: RwLock<Arc<HttpSnapshot>>,
}

impl Default for HttpClientFactory {
    fn default() -> Self {
        Self {
            current: RwLock::new(Arc::new(HttpSnapshot::new(HttpSettingsDocument::default()))),
        }
    }
}

impl HttpClientFactory {
    pub fn handle(self: &Arc<Self>, purpose: HttpPurpose) -> HttpClient {
        HttpClient {
            factory: self.clone(),
            purpose,
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<HttpSnapshot> {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn publish(&self, document: HttpSettingsDocument) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if document.revision > current.document.revision {
            *current = Arc::new(HttpSnapshot::new(document));
        }
    }
}

#[derive(Clone)]
pub struct HttpClient {
    factory: Arc<HttpClientFactory>,
    purpose: HttpPurpose,
}

impl Default for HttpClient {
    fn default() -> Self {
        Arc::new(HttpClientFactory::default()).handle(HttpPurpose::General)
    }
}

pub trait HttpClientSource: Send + Sync {
    fn http_client(&self, options: ClientOptions<'_>) -> Result<reqwest::Client, reqwest::Error>;
}

impl HttpClientSource for HttpClient {
    fn http_client(&self, options: ClientOptions<'_>) -> Result<reqwest::Client, reqwest::Error> {
        self.factory.snapshot().client(self.purpose, options)
    }
}

impl HttpClientSource for reqwest::Client {
    fn http_client(&self, options: ClientOptions<'_>) -> Result<reqwest::Client, reqwest::Error> {
        if options.proxy_url.is_none() && options.pins.is_empty() {
            return Ok(self.clone());
        }
        HttpClient::default().http_client(options)
    }
}

pub(crate) struct ScopedClient<'a> {
    source: &'a dyn HttpClientSource,
    purpose: HttpPurpose,
}

pub(crate) fn scoped(source: &dyn HttpClientSource, purpose: HttpPurpose) -> ScopedClient<'_> {
    ScopedClient { source, purpose }
}

impl HttpClientSource for ScopedClient<'_> {
    fn http_client(
        &self,
        mut options: ClientOptions<'_>,
    ) -> Result<reqwest::Client, reqwest::Error> {
        options.purpose = Some(self.purpose);
        self.source.http_client(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pools_are_isolated_by_purpose_proxy_credentials_dns_and_retry_policy() {
        let factory = Arc::new(HttpClientFactory::default());
        let source = factory.handle(HttpPurpose::General);
        let address = "127.0.0.1:8080".parse().unwrap();
        let changed = "127.0.0.1:8081".parse().unwrap();
        for _ in 0..2 {
            source.http_client(ClientOptions::default()).unwrap();
            source
                .http_client(ClientOptions {
                    purpose: Some(HttpPurpose::Oauth),
                    ..Default::default()
                })
                .unwrap();
            source
                .http_client(ClientOptions {
                    no_retry: true,
                    ..Default::default()
                })
                .unwrap();
            for password in ["fixture-one", "fixture-two"] {
                source
                    .http_client(ClientOptions {
                        proxy_url: Some("http://127.0.0.1:8088"),
                        proxy_auth: Some(("fixture-user", password)),
                        ..Default::default()
                    })
                    .unwrap();
            }
            for address in [address, changed] {
                source
                    .http_client(ClientOptions {
                        pins: &[("fixture.invalid", &[address])],
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        assert_eq!(factory.snapshot().clients.lock().unwrap().len(), 7);
        source
            .http_client(ClientOptions {
                pins: &[("fixture.invalid", &[address, address])],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(factory.snapshot().clients.lock().unwrap().len(), 7);
        let independent = HttpClientFactory::default();
        assert_eq!(independent.snapshot().clients.lock().unwrap().len(), 0);
    }
}
