use super::*;
use std::path::Path;
use tokio_rustls::{TlsAcceptor, rustls};
use wreq::tls::trust::CertStore;

pub(super) trait FixtureStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<Socket: AsyncRead + AsyncWrite + Unpin + Send> FixtureStream for Socket {}

pub(super) struct TlsFixture {
    pub(super) acceptor: TlsAcceptor,
    pub(super) roots: CertStore,
}

async fn openssl(directory: &Path, arguments: &[&str]) {
    let output = timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("openssl")
            .current_dir(directory)
            .args(arguments)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("bounded synthetic certificate generation")
    .expect("OpenSSL is required by the synthetic TLS fixture");
    assert!(
        output.status.success(),
        "synthetic certificate generation failed"
    );
}

impl TlsFixture {
    pub(super) async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        openssl(
            directory.path(),
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-keyout",
                "ca.key",
                "-out",
                "ca.pem",
                "-days",
                "1",
                "-subj",
                "/CN=MTC synthetic test root",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
            ],
        )
        .await;
        openssl(
            directory.path(),
            &[
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-keyout",
                "server.key",
                "-out",
                "server.csr",
                "-subj",
                "/CN=upload.example.test",
            ],
        )
        .await;
        tokio::fs::write(directory.path().join("server.ext"), format!(
            "subjectAltName=DNS:{HOST}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n",
        )).await.unwrap();
        openssl(
            directory.path(),
            &[
                "x509",
                "-req",
                "-in",
                "server.csr",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-set_serial",
                "2",
                "-days",
                "1",
                "-sha256",
                "-extfile",
                "server.ext",
                "-outform",
                "DER",
                "-out",
                "server.der",
            ],
        )
        .await;
        openssl(
            directory.path(),
            &[
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                "server.key",
                "-outform",
                "DER",
                "-out",
                "server-key.der",
            ],
        )
        .await;
        let roots = CertStore::from_pem_stack(
            tokio::fs::read(directory.path().join("ca.pem"))
                .await
                .unwrap(),
        )
        .unwrap();
        let certificate = rustls::pki_types::CertificateDer::from(
            tokio::fs::read(directory.path().join("server.der"))
                .await
                .unwrap(),
        );
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                tokio::fs::read(directory.path().join("server-key.der"))
                    .await
                    .unwrap(),
            ));
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            roots,
        }
    }
}

#[tokio::test]
async fn socks_tls_rejects_untrusted_fixture_certificate_before_http() {
    let fixture = TlsFixture::new().await;
    timeout(Duration::from_secs(15), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy =
            wreq::Proxy::all(format!("socks5h://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            accept_socks_at_port(&mut socket, 443).await;
            let error = fixture.acceptor.accept(socket).await.unwrap_err();
            assert!(
                matches!(
                    error
                        .get_ref()
                        .and_then(|source| source.downcast_ref::<rustls::Error>()),
                    Some(rustls::Error::AlertReceived(
                        rustls::AlertDescription::UnknownCA
                            | rustls::AlertDescription::BadCertificate
                            | rustls::AlertDescription::CertificateUnknown
                    ))
                ),
                "peer must reject the untrusted certificate before HTTP"
            );
        });
        let result = build_codex_http_client_with_policy(CodexTransportPolicy::default())
            .unwrap()
            .post(format!("https://{HOST}/v1/responses"))
            .proxy(proxy)
            .body(Bytes::from_static(PRIME))
            .send()
            .await;
        assert!(
            result.unwrap_err().is_connect(),
            "production builder must retain certificate verification"
        );
        server.await.unwrap();
    })
    .await
    .expect("bounded untrusted TLS fixture");
}
