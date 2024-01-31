use std::{
    io,
    os::fd::{AsRawFd, RawFd},
    sync::Arc,
    task,
    time::Duration,
};

use const_random::const_random;
use ktls::{AsyncReadReady, CorkStream};
use rcgen::generate_simple_self_signed;
use rustls::{
    cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    },
    client::Resumption,
    version::{TLS12, TLS13},
    ClientConfig, RootCertStore, ServerConfig, SupportedCipherSuite, SupportedProtocolVersion,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsConnector;
use tracing::{debug, Instrument};
use tracing_subscriber::EnvFilter;

const CLIENT_PAYLOAD: &[u8] = &const_random!([u8; 262144]);
const SERVER_PAYLOAD: &[u8] = &const_random!([u8; 262144]);

#[tokio::test]
async fn compatible_ciphers() {
    let cc = ktls::CompatibleCiphers::new().await.unwrap();
    for suite in [
        rustls::cipher_suite::TLS13_AES_128_GCM_SHA256,
        rustls::cipher_suite::TLS13_AES_256_GCM_SHA384,
        rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        assert!(cc.is_compatible(&suite));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compatible_ciphers_single_thread() {
    let cc = ktls::CompatibleCiphers::new().await.unwrap();
    for suite in [
        rustls::cipher_suite::TLS13_AES_128_GCM_SHA256,
        rustls::cipher_suite::TLS13_AES_256_GCM_SHA384,
        rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        assert!(cc.is_compatible(&suite));
    }
}

#[tokio::test]
async fn ktls_server_rustls_client_tls_1_3_aes_128_gcm() {
    server_test(&TLS13, TLS13_AES_128_GCM_SHA256).await;
}

#[tokio::test]
async fn ktls_server_rustls_client_tls_1_3_aes_256_gcm() {
    server_test(&TLS13, TLS13_AES_256_GCM_SHA384).await;
}

#[tokio::test]
async fn ktls_server_rustls_client_tls_1_3_chacha20_poly1305() {
    server_test(&TLS13, TLS13_CHACHA20_POLY1305_SHA256).await;
}

#[tokio::test]
async fn ktls_server_rustls_client_tls_1_2_ecdhe_aes_128_gcm() {
    server_test(&TLS12, TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256).await;
}

#[tokio::test]
async fn ktls_server_rustls_client_tls_1_2_ecdhe_aes_256_gcm() {
    server_test(&TLS12, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384).await;
}

#[tokio::test]
async fn ktls_server_rustls_client_tls_1_2_ecdhe_chacha20_poly1305() {
    server_test(&TLS12, TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256).await;
}

#[derive(Clone, Copy)]
enum ServerTestFlavor {
    ClientCloses,
    ServerCloses,
}

async fn server_test(
    protocol_version: &'static SupportedProtocolVersion,
    cipher_suite: SupportedCipherSuite,
) {
    tracing_subscriber::fmt()
        // .with_env_filter(EnvFilter::new("rustls=trace,debug"))
        // .with_env_filter(EnvFilter::new("debug"))
        .with_env_filter(EnvFilter::new("trace"))
        .pretty()
        .init();

    server_test_inner(
        protocol_version,
        cipher_suite,
        ServerTestFlavor::ClientCloses,
    )
    .await;
    server_test_inner(
        protocol_version,
        cipher_suite,
        ServerTestFlavor::ServerCloses,
    )
    .await;
}

async fn server_test_inner(
    protocol_version: &'static SupportedProtocolVersion,
    cipher_suite: SupportedCipherSuite,
    flavor: ServerTestFlavor,
) {
    let subject_alt_names = vec!["localhost".to_string()];

    let cert = generate_simple_self_signed(subject_alt_names).unwrap();
    println!("{}", cert.serialize_pem().unwrap());
    println!("{}", cert.serialize_private_key_pem());

    let mut server_config = ServerConfig::builder()
        .with_cipher_suites(&[cipher_suite])
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[protocol_version])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::Certificate(cert.serialize_der().unwrap())],
            rustls::PrivateKey(cert.serialize_private_key_der()),
        )
        .unwrap();

    server_config.enable_secret_extraction = true;
    server_config.key_log = Arc::new(rustls::KeyLogFile::new());

    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let ln = TcpListener::bind("[::]:0").await.unwrap();
    let addr = ln.local_addr().unwrap();

    let jh = tokio::spawn(
        async move {
            let (stream, addr) = ln.accept().await.unwrap();
            debug!("Accepted TCP conn from {}", addr);
            let stream = SpyStream(stream, "server");
            let stream = CorkStream::new(stream);

            let stream = acceptor.accept(stream).await.unwrap();
            debug!("Completed TLS handshake");

            // sleep for a bit to let client write more data and stress test
            // the draining logic
            tokio::time::sleep(Duration::from_millis(100)).await;

            let mut stream = ktls::config_ktls_server(stream).await.unwrap();
            debug!("Configured kTLS");

            debug!("Server reading data (1/5)");
            let mut buf = vec![0u8; CLIENT_PAYLOAD.len()];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, CLIENT_PAYLOAD);

            debug!("Server writing data (2/5)");
            stream.write_all(SERVER_PAYLOAD).await.unwrap();
            stream.flush().await.unwrap();

            debug!("Server reading data (3/5)");
            let mut buf = vec![0u8; CLIENT_PAYLOAD.len()];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, CLIENT_PAYLOAD);

            debug!("Server writing data (4/5)");
            stream.write_all(SERVER_PAYLOAD).await.unwrap();
            stream.flush().await.unwrap();

            match flavor {
                ServerTestFlavor::ClientCloses => {
                    debug!("Server reading from closed session (5/5)");
                    assert!(
                        stream.read_exact(&mut buf[..1]).await.is_err(),
                        "Session still open?"
                    );
                }
                ServerTestFlavor::ServerCloses => {
                    debug!("Server sending close notify (5/5)");
                    stream.shutdown().await.unwrap();

                    debug!("Server trying to write after closing");
                    stream.write_all(SERVER_PAYLOAD).await.unwrap_err();
                }
            }

            assert_eq!(stream.get_ref().1, "server");
            assert_eq!(stream.get_mut().1, "server");
            assert_eq!(stream.into_raw().1 .1, "server");
        }
        .instrument(tracing::info_span!("server")),
    );

    let mut root_certs = RootCertStore::empty();
    root_certs
        .add(&rustls::Certificate(cert.serialize_der().unwrap()))
        .unwrap();

    let client_config = ClientConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(root_certs)
        .with_no_client_auth();

    let tls_connector = TlsConnector::from(Arc::new(client_config));

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut stream = tls_connector
        .connect("localhost".try_into().unwrap(), stream)
        .await
        .unwrap();

    debug!("Client writing data (1/5)");
    stream.write_all(CLIENT_PAYLOAD).await.unwrap();
    debug!("Flushing");
    stream.flush().await.unwrap();

    debug!("Client reading data (2/5)");
    let mut buf = vec![0u8; SERVER_PAYLOAD.len()];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf, SERVER_PAYLOAD);

    debug!("Client writing data (3/5)");
    stream.write_all(CLIENT_PAYLOAD).await.unwrap();
    debug!("Flushing");
    stream.flush().await.unwrap();

    debug!("Client reading data (4/5)");
    let mut buf = vec![0u8; SERVER_PAYLOAD.len()];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf, SERVER_PAYLOAD);

    match flavor {
        ServerTestFlavor::ClientCloses => {
            debug!("Client sending close notify (5/5)");
            stream.shutdown().await.unwrap();

            debug!("Client trying to write after closing");
            stream.write_all(CLIENT_PAYLOAD).await.unwrap_err();
        }
        ServerTestFlavor::ServerCloses => {
            debug!("Client reading from closed session (5/5)");
            assert!(
                stream.read_exact(&mut buf[..1]).await.is_err(),
                "Session still open?"
            );
        }
    }

    jh.await.unwrap();
}

#[tokio::test]
async fn ktls_client_rustls_server_tls_1_3_aes_128_gcm() {
    client_test(&TLS13, TLS13_AES_128_GCM_SHA256).await;
}

#[tokio::test]
async fn ktls_client_rustls_server_tls_1_3_aes_256_gcm() {
    client_test(&TLS13, TLS13_AES_256_GCM_SHA384).await;
}

#[tokio::test]
async fn ktls_client_rustls_server_tls_1_3_chacha20_poly1305() {
    client_test(&TLS13, TLS13_CHACHA20_POLY1305_SHA256).await;
}

#[tokio::test]
async fn ktls_client_rustls_server_tls_1_2_ecdhe_aes_128_gcm() {
    client_test(&TLS12, TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256).await;
}

#[tokio::test]
async fn ktls_client_rustls_server_tls_1_2_ecdhe_aes_256_gcm() {
    client_test(&TLS12, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384).await;
}

#[tokio::test]
async fn ktls_client_rustls_server_tls_1_2_ecdhe_chacha20_poly1305() {
    client_test(&TLS12, TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256).await;
}

enum ClientTestFlavor {
    ShortLastBuffer,
    LongLastBuffer,
}

async fn client_test(
    protocol_version: &'static SupportedProtocolVersion,
    cipher_suite: SupportedCipherSuite,
) {
    tracing_subscriber::fmt()
        // .with_env_filter(EnvFilter::new("rustls=trace,debug"))
        // .with_env_filter(EnvFilter::new("debug"))
        .with_env_filter(EnvFilter::new("trace"))
        .pretty()
        .init();

    client_test_inner(
        protocol_version,
        cipher_suite,
        ClientTestFlavor::ShortLastBuffer,
    )
    .await;
    client_test_inner(
        protocol_version,
        cipher_suite,
        ClientTestFlavor::LongLastBuffer,
    )
    .await;
}

async fn client_test_inner(
    protocol_version: &'static SupportedProtocolVersion,
    cipher_suite: SupportedCipherSuite,
    flavor: ClientTestFlavor,
) {
    let subject_alt_names = vec!["localhost".to_string()];

    let cert = generate_simple_self_signed(subject_alt_names).unwrap();
    println!("{}", cert.serialize_pem().unwrap());
    println!("{}", cert.serialize_private_key_pem());

    let mut server_config = ServerConfig::builder()
        .with_cipher_suites(&[cipher_suite])
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[protocol_version])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::Certificate(cert.serialize_der().unwrap())],
            rustls::PrivateKey(cert.serialize_private_key_der()),
        )
        .unwrap();

    server_config.key_log = Arc::new(rustls::KeyLogFile::new());
    // server_config.send_tls13_tickets = 0;

    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let ln = TcpListener::bind("[::]:0").await.unwrap();
    let addr = ln.local_addr().unwrap();

    let jh = tokio::spawn(
        async move {
            let (stream, addr) = ln.accept().await.unwrap();

            debug!("Accepted TCP conn from {}", addr);
            let mut stream = acceptor.accept(stream).await.unwrap();
            debug!("Completed TLS handshake");

            debug!("Server reading data (1/5)");
            let mut buf = vec![0u8; CLIENT_PAYLOAD.len()];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, CLIENT_PAYLOAD);

            debug!("Server writing data (2/5)");
            stream.write_all(SERVER_PAYLOAD).await.unwrap();

            debug!("Server reading data (3/5)");
            let mut buf = vec![0u8; CLIENT_PAYLOAD.len()];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, CLIENT_PAYLOAD);

            for _i in 0..3 {
                debug!("Making the client wait (to make busywaits REALLY obvious)");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            debug!("Server writing data (4/5)");
            stream.write_all(SERVER_PAYLOAD).await.unwrap();

            debug!("Server sending close notify (5/5)");
            stream.shutdown().await.unwrap();

            debug!("Server trying to write after close notify");
            stream.write_all(SERVER_PAYLOAD).await.unwrap_err();

            debug!("Server is happy with the exchange");
        }
        .instrument(tracing::info_span!("server")),
    );

    let mut root_certs = RootCertStore::empty();
    root_certs
        .add(&rustls::Certificate(cert.serialize_der().unwrap()))
        .unwrap();

    let mut client_config = ClientConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(root_certs)
        .with_no_client_auth();

    client_config.enable_secret_extraction = true;
    client_config.resumption = Resumption::disabled();

    let tls_connector = TlsConnector::from(Arc::new(client_config));

    let stream = TcpStream::connect(addr).await.unwrap();
    let stream = CorkStream::new(stream);

    let stream = tls_connector
        .connect("localhost".try_into().unwrap(), stream)
        .await
        .unwrap();

    let stream = ktls::config_ktls_client(stream).await.unwrap();
    let mut stream = SpyStream(stream, "client");

    debug!("Client writing data (1/5)");
    stream.write_all(CLIENT_PAYLOAD).await.unwrap();
    debug!("Flushing");
    stream.flush().await.unwrap();

    tokio::time::sleep(Duration::from_millis(250)).await;

    debug!("Client reading data (2/5)");
    let mut buf = vec![0u8; SERVER_PAYLOAD.len()];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf, SERVER_PAYLOAD);

    debug!("Client writing data (3/5)");
    stream.write_all(CLIENT_PAYLOAD).await.unwrap();
    debug!("Flushing");
    stream.flush().await.unwrap();

    debug!("Client reading data (4/5)");
    let mut buf = vec![0u8; SERVER_PAYLOAD.len()];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf, SERVER_PAYLOAD);

    let buf = match flavor {
        ClientTestFlavor::ShortLastBuffer => &mut buf[..1],
        ClientTestFlavor::LongLastBuffer => &mut buf[..2],
    };
    debug!(
        "Client reading from closed session (with buffer of size {})",
        buf.len()
    );
    assert!(stream.read_exact(buf).await.is_err(), "Session still open?");

    jh.await.unwrap();
}

struct SpyStream<IO>(IO, &'static str);

impl<IO> AsyncRead for SpyStream<IO>
where
    IO: AsyncRead,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> task::Poll<std::io::Result<()>> {
        let old_filled = buf.filled().len();
        let name = self.1;
        let res = unsafe {
            let io = self.map_unchecked_mut(|s| &mut s.0);
            io.poll_read(cx, buf)
        };

        match &res {
            task::Poll::Ready(res) => match res {
                Ok(_) => {
                    let num_read = buf.filled().len() - old_filled;
                    tracing::debug!(%name, "SpyStream read {num_read} bytes",);
                }
                Err(e) => {
                    tracing::debug!(%name, "SpyStream read errored: {e}");
                }
            },
            task::Poll::Pending => {
                tracing::debug!(%name, "SpyStream read would've blocked")
            }
        }
        res
    }
}

impl<IO> AsyncReadReady for SpyStream<IO>
where
    IO: AsyncReadReady,
{
    fn poll_read_ready(&self, cx: &mut task::Context<'_>) -> task::Poll<io::Result<()>> {
        self.0.poll_read_ready(cx)
    }
}

impl<IO> AsyncWrite for SpyStream<IO>
where
    IO: AsyncWrite,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> task::Poll<Result<usize, std::io::Error>> {
        let res = unsafe {
            let io = self.map_unchecked_mut(|s| &mut s.0);
            io.poll_write(cx, buf)
        };

        match &res {
            task::Poll::Ready(res) => match res {
                Ok(n) => {
                    tracing::debug!("SpyStream wrote {n} bytes");
                }
                Err(e) => {
                    tracing::debug!("SpyStream writing errored: {e}");
                }
            },
            task::Poll::Pending => {
                tracing::debug!("SpyStream writing would've blocked")
            }
        }
        res
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), std::io::Error>> {
        unsafe {
            let io = self.map_unchecked_mut(|s| &mut s.0);
            io.poll_flush(cx)
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), std::io::Error>> {
        unsafe {
            let io = self.map_unchecked_mut(|s| &mut s.0);
            io.poll_shutdown(cx)
        }
    }
}

impl<IO> AsRawFd for SpyStream<IO>
where
    IO: AsRawFd,
{
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
