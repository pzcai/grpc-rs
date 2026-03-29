//! gRPC server core: service registration, TCP listener, and stream dispatch.
//!
//! ## Dispatch model (mirrors grpc-go's `server.go`)
//!
//! 1. `Server::serve` binds a TCP listener and spawns a task per connection.
//! 2. Per-connection task creates a `ServerTransport` (HTTP/2) and loops on
//!    `transport.accept()`, spawning a task per RPC stream.
//! 3. Per-stream task:
//!    a. Validates `content-type` starts with `"application/grpc"`.
//!    b. Parses the method path `/{service}/{method}`.
//!    c. Looks up the registered service and method handler.
//!    d. Reads the request frame, decodes the payload, calls the handler.
//!    e. Encodes the response payload, sends headers + data + trailers.
//!
//! ## Handler contract
//!
//! Unary handlers receive the raw serialized request bytes (protobuf, no gRPC
//! framing) and return raw serialized response bytes.  The server core handles
//! framing (the 5-byte length-prefix) via the codec layer.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http::HeaderMap;
use tokio::net::TcpListener;

use crate::codec;
use crate::interceptor::{self, UnaryServerInterceptor};
use crate::metadata::Metadata;
use crate::status::Status;
use crate::tls;
use crate::transport::{self, ServerTransport};

// Re-export ServerStream so callers can use it without depending on the transport module directly.
pub use crate::transport::ServerStream;

// ── Handler types ─────────────────────────────────────────────────────────────

/// A pinned, boxed future used as the return type of async handler closures.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A unary RPC handler.
///
/// - First arg: decoded request payload (raw protobuf bytes, no gRPC framing).
/// - Second arg: request metadata (user-defined headers, gRPC-internal headers stripped).
/// - Output: encoded response payload (raw protobuf bytes, no gRPC framing), or a
///   [`Status`] error.
///
/// Implementations must be `Send + Sync` so they can be shared across tasks.
pub type UnaryHandlerFn =
    Arc<dyn Fn(Bytes, Metadata) -> BoxFuture<Result<Bytes, Status>> + Send + Sync + 'static>;

/// A streaming RPC handler.
///
/// Receives the raw `ServerStream` (after header validation and metadata extraction)
/// plus the request metadata.  The handler is responsible for calling `recv_message`,
/// `send_headers`, `send_message`, and `send_trailers` on the stream itself.
pub type StreamingHandlerFn =
    Arc<dyn Fn(ServerStream, Metadata) -> BoxFuture<()> + Send + Sync + 'static>;

/// Discriminates between unary and streaming method handlers.
pub enum Handler {
    Unary(UnaryHandlerFn),
    Streaming(StreamingHandlerFn),
}

// ── Service description ───────────────────────────────────────────────────────

/// Descriptor for a single RPC method (unary or streaming).
pub struct MethodDesc {
    /// The bare method name, e.g. `"SayHello"`.
    pub name: &'static str,
    pub handler: Handler,
}

/// Descriptor for a gRPC service.
pub struct ServiceDesc {
    /// The full service name including package, e.g. `"helloworld.Greeter"`.
    pub name: &'static str,
    pub methods: Vec<MethodDesc>,
}

// ── Server ────────────────────────────────────────────────────────────────────

struct ServiceEntry {
    methods: HashMap<&'static str, Handler>,
}

/// A gRPC server.
///
/// Register services with [`add_service`](Server::add_service), optionally add
/// interceptors with [`add_interceptor`](Server::add_interceptor), then start
/// serving with [`serve`](Server::serve).
pub struct Server {
    services: HashMap<&'static str, ServiceEntry>,
    interceptors: Vec<UnaryServerInterceptor>,
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}

impl Server {
    pub fn new() -> Self {
        Server {
            services: HashMap::new(),
            interceptors: Vec::new(),
        }
    }

    /// Register a unary server interceptor.
    ///
    /// Interceptors are applied in registration order (first registered = outermost).
    /// Only affects unary RPC handlers; streaming handlers receive the stream directly.
    pub fn add_interceptor(&mut self, interceptor: UnaryServerInterceptor) {
        self.interceptors.push(interceptor);
    }

    /// Register a service. Panics if a service with the same name is already registered.
    pub fn add_service(&mut self, desc: ServiceDesc) {
        assert!(
            !self.services.contains_key(desc.name),
            "service '{}' already registered",
            desc.name
        );
        let mut methods = HashMap::new();
        for m in desc.methods {
            methods.insert(m.name, m.handler);
        }
        self.services.insert(desc.name, ServiceEntry { methods });
    }

    /// Bind `addr` and serve until the returned future is dropped or the listener errors.
    ///
    /// Each accepted TCP connection runs in its own task.  Each RPC stream within a
    /// connection runs in its own task.
    pub async fn serve(self, addr: SocketAddr) -> Result<(), Status> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Status::internal(format!("bind {addr}: {e}")))?;

        let services = Arc::new(self.services);
        let interceptors = Arc::new(self.interceptors);

        loop {
            let (tcp, _peer) = listener
                .accept()
                .await
                .map_err(|e| Status::internal(format!("TCP accept: {e}")))?;

            let services = Arc::clone(&services);
            let interceptors = Arc::clone(&interceptors);
            tokio::spawn(async move {
                let mut transport = match ServerTransport::new(tcp).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                while let Some(stream) = transport.accept().await {
                    let services = Arc::clone(&services);
                    let interceptors = Arc::clone(&interceptors);
                    tokio::spawn(dispatch_stream(stream, services, interceptors));
                }
            });
        }
    }

    /// Bind `addr` with TLS and serve until the returned future is dropped.
    ///
    /// - `tls_cfg`: a rustls `ServerConfig`; ALPN `h2` must be included
    ///   (use [`crate::tls::server_config_from_cert`] to build one).
    pub async fn serve_tls(self, addr: SocketAddr, tls_cfg: tls::ServerConfig) -> Result<(), Status> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Status::internal(format!("bind {addr}: {e}")))?;

        let acceptor = tls::acceptor(tls_cfg);
        let services = Arc::new(self.services);
        let interceptors = Arc::new(self.interceptors);

        loop {
            let (tcp, _peer) = listener
                .accept()
                .await
                .map_err(|e| Status::internal(format!("TCP accept: {e}")))?;

            let acceptor = acceptor.clone();
            let services = Arc::clone(&services);
            let interceptors = Arc::clone(&interceptors);
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(tcp).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut transport = match ServerTransport::new(tls_stream).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                while let Some(stream) = transport.accept().await {
                    let services = Arc::clone(&services);
                    let interceptors = Arc::clone(&interceptors);
                    tokio::spawn(dispatch_stream(stream, services, interceptors));
                }
            });
        }
    }
}

// ── Per-stream dispatch ───────────────────────────────────────────────────────

async fn dispatch_stream(
    mut stream: ServerStream,
    services: Arc<HashMap<&'static str, ServiceEntry>>,
    interceptors: Arc<Vec<UnaryServerInterceptor>>,
) {
    // Extract all data we need from the stream headers up-front (owned),
    // so we can take &mut stream for error responses without borrow conflicts.
    let method_path = stream.method().to_owned();
    let content_type = stream
        .request_headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    // Parse optional grpc-timeout header → server-side deadline.
    let deadline: Option<std::time::Duration> = stream
        .request_headers()
        .get("grpc-timeout")
        .and_then(|v| v.to_str().ok())
        .and_then(transport::decode_timeout);
    // Extract user-defined request metadata (strips transport/gRPC-internal headers).
    let request_metadata = Metadata::from_request_headers(stream.request_headers());

    // Validate content-type: must start with "application/grpc"
    if !content_type.starts_with("application/grpc") {
        send_error(
            &mut stream,
            Status::invalid_argument(format!(
                "invalid content-type {content_type:?}: must start with \"application/grpc\""
            )),
        );
        return;
    }

    // Parse method path: "/{service}/{method}"
    // grpc-go: strip leading '/', then split at last '/'
    let (service_name, method_name) = match parse_method_path(&method_path) {
        Some((s, m)) => (s.to_owned(), m.to_owned()),
        None => {
            send_error(
                &mut stream,
                Status::unimplemented(format!(
                    "malformed method path: {method_path:?}"
                )),
            );
            return;
        }
    };

    // Look up service
    let service = match services.get(service_name.as_str()) {
        Some(s) => s,
        None => {
            send_error(
                &mut stream,
                Status::unimplemented(format!("unknown service {service_name}")),
            );
            return;
        }
    };

    // Look up method
    let handler = match service.methods.get(method_name.as_str()) {
        Some(h) => h,
        None => {
            send_error(
                &mut stream,
                Status::unimplemented(format!(
                    "unknown method {method_name} in service {service_name}"
                )),
            );
            return;
        }
    };

    match handler {
        Handler::Unary(handler) => {
            dispatch_unary(stream, handler.clone(), request_metadata, deadline, &interceptors, method_path.clone()).await;
        }
        Handler::Streaming(handler) => {
            // Streaming handlers own the stream entirely; dispatch directly.
            let handler = handler.clone();
            if let Some(d) = deadline {
                tokio::time::timeout(d, handler(stream, request_metadata))
                    .await
                    .ok();
            } else {
                handler(stream, request_metadata).await;
            }
        }
    }
}

async fn dispatch_unary(
    mut stream: ServerStream,
    handler: UnaryHandlerFn,
    request_metadata: Metadata,
    deadline: Option<std::time::Duration>,
    interceptors: &[UnaryServerInterceptor],
    method_path: String,
) {
    // Read the request frame (first message; unary RPC has exactly one)
    let frame = match stream.recv_message().await {
        Ok(Some(f)) => f,
        Ok(None) => {
            send_error(&mut stream, Status::internal("empty request stream"));
            return;
        }
        Err(e) => {
            send_error(&mut stream, e);
            return;
        }
    };

    // Decode: strip the 5-byte gRPC frame header → raw proto payload
    let payload = match codec::decode_raw(&frame, None, codec::DEFAULT_MAX_RECV_SIZE) {
        Ok(p) => Bytes::from(p),
        Err(e) => {
            send_error(&mut stream, e);
            return;
        }
    };

    // Build the interceptor chain around the handler (or use handler directly if no interceptors).
    let terminal: interceptor::ServerNext = Arc::new(move |req, md| handler(req, md));
    let chain = interceptor::chain_server(interceptors, method_path, terminal);

    // Invoke chain with optional server-side deadline.
    let handler_result = if let Some(d) = deadline {
        match tokio::time::timeout(d, chain(payload, request_metadata)).await {
            Ok(r) => r,
            Err(_) => Err(Status::deadline_exceeded("server-side deadline exceeded")),
        }
    } else {
        chain(payload, request_metadata).await
    };
    let response_payload = match handler_result {
        Ok(b) => b,
        Err(e) => {
            send_error(&mut stream, e);
            return;
        }
    };

    // Encode: add 5-byte gRPC frame header → response frame
    let response_frame = match codec::encode_raw(&response_payload, None) {
        Ok(f) => f,
        Err(e) => {
            send_error(&mut stream, e);
            return;
        }
    };

    // Send response headers + data + trailers
    if let Err(e) = stream.send_headers(&HeaderMap::new()) {
        let _ = stream.send_trailers(&e, &HeaderMap::new());
        return;
    }
    if let Err(e) = stream.send_message(response_frame) {
        let _ = stream.send_trailers(&e, &HeaderMap::new());
        return;
    }
    // Ignore error from send_trailers (client may have disconnected)
    stream.send_trailers(&Status::ok(), &HeaderMap::new()).ok();
}

/// Send a trailer-only error response (auto-sends initial headers).
fn send_error(stream: &mut ServerStream, status: Status) {
    stream.send_trailers(&status, &HeaderMap::new()).ok();
}

// ── Path parsing ──────────────────────────────────────────────────────────────

/// Parse a gRPC method path into `(service_name, method_name)`.
///
/// Valid format: `"/{service}/{method}"` where both parts are non-empty.
/// Matches grpc-go's `handleStream`: strip leading `/`, split at last `/`.
pub fn parse_method_path(path: &str) -> Option<(&str, &str)> {
    let path = path.strip_prefix('/')?;
    let slash = path.rfind('/')?;
    let service = &path[..slash];
    let method = &path[slash + 1..];
    if service.is_empty() || method.is_empty() {
        return None;
    }
    Some((service, method))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_method_path ────────────────────────────────────────────────────

    #[test]
    fn parse_standard_path() {
        let (svc, method) = parse_method_path("/helloworld.Greeter/SayHello").unwrap();
        assert_eq!(svc, "helloworld.Greeter");
        assert_eq!(method, "SayHello");
    }

    #[test]
    fn parse_no_package_path() {
        let (svc, method) = parse_method_path("/Greeter/SayHello").unwrap();
        assert_eq!(svc, "Greeter");
        assert_eq!(method, "SayHello");
    }

    #[test]
    fn parse_no_leading_slash_returns_none() {
        assert!(parse_method_path("helloworld.Greeter/SayHello").is_none());
    }

    #[test]
    fn parse_missing_method_returns_none() {
        assert!(parse_method_path("/helloworld.Greeter/").is_none());
    }

    #[test]
    fn parse_missing_service_returns_none() {
        assert!(parse_method_path("//SayHello").is_none());
    }

    #[test]
    fn parse_no_slash_returns_none() {
        assert!(parse_method_path("/SayHello").is_none());
    }

    // ── Server dispatch integration ──────────────────────────────────────────

    /// Helper: build a simple echo unary handler that returns its input unchanged.
    fn echo_handler() -> UnaryHandlerFn {
        Arc::new(|req: Bytes, _md: Metadata| {
            Box::pin(async move { Ok(req) })
        })
    }

    /// Helper: bind a Server with `echo_handler` on `test.Echo/Echo`, start it,
    /// return the bound address.
    async fn start_echo_server() -> SocketAddr {
        let mut server = Server::new();
        server.add_service(ServiceDesc {
            name: "test.Echo",
            methods: vec![MethodDesc {
                name: "Echo",
                handler: Handler::Unary(echo_handler()),
            }],
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // release port; Server::serve will re-bind

        // Spawn the server as a background task
        tokio::spawn(async move {
            server.serve(addr).await.ok();
        });

        // Give the server task a moment to bind
        tokio::task::yield_now().await;
        addr
    }

    /// End-to-end test: ClientTransport → Server → echo handler.
    #[tokio::test]
    async fn unary_rpc_round_trip() {
        let addr = start_echo_server().await;

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut transport, conn_fut) =
            crate::transport::ClientTransport::connect(tcp).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport
            .new_stream("/test.Echo/Echo", &HeaderMap::new())
            .unwrap();

        let request_payload = Bytes::from_static(b"hello from client");
        let frame = codec::encode_raw(&request_payload, None).unwrap();
        stream.send_message(frame, true).unwrap();

        stream.recv_headers().await.unwrap();
        let resp_frame = stream.recv_message().await.unwrap().unwrap();
        let resp_payload =
            codec::decode_raw(&resp_frame, None, codec::DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(resp_payload, request_payload.as_ref());

        let (status, _) = stream.recv_trailers().await.unwrap();
        assert!(status.is_ok());
    }

    /// Unknown service returns Unimplemented.
    #[tokio::test]
    async fn unknown_service_returns_unimplemented() {
        let addr = start_echo_server().await;

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut transport, conn_fut) =
            crate::transport::ClientTransport::connect(tcp).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport
            .new_stream("/no.Such.Service/Method", &HeaderMap::new())
            .unwrap();
        stream
            .send_message(codec::encode_raw(b"x", None).unwrap(), true)
            .unwrap();

        stream.recv_headers().await.unwrap();
        let (status, _) = stream.recv_trailers().await.unwrap();
        assert_eq!(status.code, crate::status::Code::Unimplemented);
    }

    /// Unknown method returns Unimplemented.
    #[tokio::test]
    async fn unknown_method_returns_unimplemented() {
        let addr = start_echo_server().await;

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut transport, conn_fut) =
            crate::transport::ClientTransport::connect(tcp).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport
            .new_stream("/test.Echo/NoSuchMethod", &HeaderMap::new())
            .unwrap();
        stream
            .send_message(codec::encode_raw(b"x", None).unwrap(), true)
            .unwrap();

        stream.recv_headers().await.unwrap();
        let (status, _) = stream.recv_trailers().await.unwrap();
        assert_eq!(status.code, crate::status::Code::Unimplemented);
    }

    /// Handler error propagates as gRPC status.
    #[tokio::test]
    async fn handler_error_propagates() {
        let mut server = Server::new();
        server.add_service(ServiceDesc {
            name: "test.Fail",
            methods: vec![MethodDesc {
                name: "Fail",
                handler: Handler::Unary(Arc::new(|_: Bytes, _md: Metadata| {
                    Box::pin(async move {
                        Err(Status::not_found("thing not found"))
                    })
                })),
            }],
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        tokio::spawn(async move { server.serve(addr).await.ok(); });
        tokio::task::yield_now().await;

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut transport, conn_fut) =
            crate::transport::ClientTransport::connect(tcp).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport
            .new_stream("/test.Fail/Fail", &HeaderMap::new())
            .unwrap();
        stream
            .send_message(codec::encode_raw(b"x", None).unwrap(), true)
            .unwrap();

        stream.recv_headers().await.unwrap();
        let (status, _) = stream.recv_trailers().await.unwrap();
        assert_eq!(status.code, crate::status::Code::NotFound);
        assert_eq!(status.message, "thing not found");
    }
}
