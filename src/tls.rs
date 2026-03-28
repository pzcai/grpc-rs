//! TLS support for gRPC-rs using rustls.
//!
//! Provides helper constructors for building `rustls` client and server
//! configurations pre-configured for HTTP/2 (the `h2` ALPN token required
//! by gRPC).
//!
//! ## ALPN
//!
//! gRPC over TLS requires the TLS handshake to advertise and negotiate `"h2"`
//! via ALPN (Application-Layer Protocol Negotiation).  The helpers here set
//! this automatically.

use std::sync::Arc;

pub use rustls::{ClientConfig, ServerConfig};
pub use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// The ALPN token for HTTP/2, required by gRPC.
pub const H2_ALPN: &[u8] = b"h2";

/// Build a `ClientConfig` from a `RootCertStore`, with ALPN `h2` pre-set.
///
/// The caller can customise the config before wrapping in an `Arc`.
pub fn client_config_from_roots(
    roots: rustls::RootCertStore,
) -> ClientConfig {
    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![H2_ALPN.to_vec()];
    cfg
}

/// Build a `ServerConfig` from a certificate chain and private key, with
/// ALPN `h2` pre-set.
///
/// - `cert_chain`: DER-encoded certificate chain (leaf first).
/// - `key`: DER-encoded private key.
pub fn server_config_from_cert(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig, rustls::Error> {
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    cfg.alpn_protocols = vec![H2_ALPN.to_vec()];
    Ok(cfg)
}

/// Re-export `ServerName` for use in `Channel::connect_tls`.
pub use rustls::pki_types::ServerName as TlsServerName;

/// Wrap a `rustls::ClientConfig` in an `Arc` for use with
/// `tokio_rustls::TlsConnector`.
pub fn connector(cfg: ClientConfig) -> tokio_rustls::TlsConnector {
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
}

/// Wrap a `rustls::ServerConfig` in an `Arc` for use with
/// `tokio_rustls::TlsAcceptor`.
pub fn acceptor(cfg: ServerConfig) -> tokio_rustls::TlsAcceptor {
    tokio_rustls::TlsAcceptor::from(Arc::new(cfg))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bytes::Bytes;
    use rustls::pki_types::ServerName;
    use tokio::net::TcpListener;

    use crate::metadata::Metadata;
    use crate::server::{Handler, MethodDesc, Server, ServiceDesc, UnaryHandlerFn};
    /// Generate a self-signed cert for `localhost` using `rcgen`.
    fn make_self_signed() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let key_der = PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            cert.key_pair.serialize_der(),
        ));
        (vec![cert_der], key_der)
    }

    fn echo_handler() -> UnaryHandlerFn {
        Arc::new(|req: Bytes, _md: Metadata| Box::pin(async move { Ok(req) }))
    }

    #[tokio::test]
    async fn tls_unary_round_trip() {
        use crate::client::Channel;
        use crate::codec;
        use crate::transport::ClientTransport;

        // Install the ring CryptoProvider (required once per process with rustls 0.23).
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Build self-signed cert.
        let (cert_chain, key) = make_self_signed();

        // Server TLS config.
        let server_cfg = server_config_from_cert(cert_chain.clone(), key).unwrap();

        // Client TLS config: trust the self-signed cert.
        let mut roots = rustls::RootCertStore::empty();
        for cert in &cert_chain {
            roots.add(cert.clone()).unwrap();
        }
        let client_cfg = client_config_from_roots(roots);

        // Bind the server.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let mut server = Server::new();
        server.add_service(ServiceDesc {
            name: "test.Tls",
            methods: vec![MethodDesc { name: "Echo", handler: Handler::Unary(echo_handler()) }],
        });
        tokio::spawn(async move { server.serve_tls(addr, server_cfg).await.ok(); });
        tokio::task::yield_now().await;

        // Connect with TLS.
        let channel = Channel::connect_tls(
            addr,
            ServerName::try_from("localhost").unwrap().to_owned(),
            client_cfg,
        ).await.unwrap();

        // Make a unary call using the raw transport path (echo handler returns raw bytes).
        // Use ClientTransport directly so we can verify raw bytes round-trip.
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut roots2 = rustls::RootCertStore::empty();
        for cert in &cert_chain {
            roots2.add(cert.clone()).unwrap();
        }
        let client_cfg2 = client_config_from_roots(roots2);
        let tls_stream = connector(client_cfg2)
            .connect(ServerName::try_from("localhost").unwrap().to_owned(), tcp)
            .await
            .unwrap();
        let (mut transport, conn_fut) = ClientTransport::connect(tls_stream).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport.new_stream("/test.Tls/Echo", &http::HeaderMap::new()).unwrap();
        let payload = b"hello tls";
        let frame = codec::encode_raw(payload, None).unwrap();
        stream.send_message(frame, true).unwrap();

        stream.recv_headers().await.unwrap();
        let resp_frame = stream.recv_message().await.unwrap().unwrap();
        let resp = codec::decode_raw(&resp_frame, None, codec::DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(resp, payload);

        let (status, _) = stream.recv_trailers().await.unwrap();
        assert!(status.is_ok());

        // Keep channel alive until the end of test
        drop(channel);
    }
}
