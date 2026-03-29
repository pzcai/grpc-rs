//! gRPC client core: `Channel` for making RPC calls.
//!
//! ## Design
//!
//! `Channel` wraps a single `ClientTransport` (one HTTP/2 connection) behind a
//! `std::sync::Mutex`.  The mutex is only held for the synchronous `new_stream`
//! call; it is released before any `.await`, so there is no risk of holding a
//! `std::sync::Mutex` across an await point.
//!
//! This is a minimal Channel implementation (no reconnection, no name resolution,
//! no load balancing).  These features are deferred until a later session.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpStream;

use crate::codec;
use crate::interceptor::{self, UnaryClientInterceptor};
use crate::metadata::Metadata;
use crate::retry::RetryPolicy;
use crate::status::Status;
use crate::tls;
use crate::transport::{self, ClientTransport};

/// A gRPC channel: a logical connection to a single server endpoint.
///
/// Cheaply cloneable in the future (when backed by a connection pool); for now
/// it holds one HTTP/2 connection.
pub struct Channel {
    transport: Arc<Mutex<ClientTransport>>,
    interceptors: Vec<UnaryClientInterceptor>,
    retry_policy: Option<RetryPolicy>,
}

impl Channel {
    /// Connect to a gRPC server at `addr` over plain TCP (no TLS).
    ///
    /// The HTTP/2 connection task is spawned internally.
    pub async fn connect(addr: SocketAddr) -> Result<Self, Status> {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| Status::unavailable(format!("connect {addr}: {e}")))?;

        let (transport, conn_fut) = ClientTransport::connect(tcp).await?;
        tokio::spawn(conn_fut);

        Ok(Channel {
            transport: Arc::new(Mutex::new(transport)),
            interceptors: Vec::new(),
            retry_policy: None,
        })
    }

    /// Connect to a gRPC server at `addr` over TLS.
    ///
    /// - `server_name`: the SNI hostname for TLS certificate verification.
    /// - `tls_cfg`: a rustls `ClientConfig`; ALPN `h2` must be included
    ///   (use [`crate::tls::client_config_from_roots`] to build one).
    pub async fn connect_tls(
        addr: SocketAddr,
        server_name: tls::TlsServerName<'static>,
        tls_cfg: tls::ClientConfig,
    ) -> Result<Self, Status> {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| Status::unavailable(format!("connect {addr}: {e}")))?;

        let connector = tls::connector(tls_cfg);
        let tls_stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| Status::unavailable(format!("TLS handshake: {e}")))?;

        let (transport, conn_fut) = ClientTransport::connect(tls_stream).await?;
        tokio::spawn(conn_fut);

        Ok(Channel {
            transport: Arc::new(Mutex::new(transport)),
            interceptors: Vec::new(),
            retry_policy: None,
        })
    }

    /// Attach a retry policy to this channel.  Retries apply to unary RPCs only.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = Some(policy);
        self
    }

    /// Register a client-side unary interceptor.
    ///
    /// Interceptors are applied in registration order (first registered = outermost).
    pub fn add_interceptor(&mut self, interceptor: UnaryClientInterceptor) {
        self.interceptors.push(interceptor);
    }

    /// Make a unary RPC call.
    ///
    /// - `method`: the full method path, e.g. `"/helloworld.Greeter/SayHello"`.
    /// - `req`: the request message (serialized with prost).
    /// - `metadata`: request metadata (user-defined headers).
    /// - `timeout`: optional deadline for the entire call; returns
    ///   `Status::deadline_exceeded` if it fires.
    ///
    /// Returns `(response, trailing_metadata)` on success, or a [`Status`] error.
    /// The trailing metadata contains any user-defined headers the server sent in
    /// its trailing HEADERS frame (excludes `grpc-status` and `grpc-message`).
    pub async fn call_unary<Req, Resp>(
        &self,
        method: &str,
        req: &Req,
        metadata: &Metadata,
        timeout: Option<Duration>,
    ) -> Result<(Resp, Metadata), Status>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        let inner_fut = async {
            let max_attempts = self
                .retry_policy
                .as_ref()
                .map(|p| p.max_attempts.max(1).min(5))
                .unwrap_or(1);

            let mut last_err = Status::internal("no attempts made");

            for attempt in 0..max_attempts {
                // Exponential backoff before every attempt after the first.
                if attempt > 0 {
                    let backoff = self
                        .retry_policy
                        .as_ref()
                        .map(|p| p.backoff_for_attempt(attempt))
                        .unwrap_or(Duration::ZERO);
                    if !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                }

                match self.call_unary_inner(method, req, metadata, timeout).await {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        let retryable = self
                            .retry_policy
                            .as_ref()
                            .map(|p| p.is_retryable(e.code))
                            .unwrap_or(false);
                        last_err = e;
                        if !retryable {
                            break;
                        }
                        // retryable: continue to next attempt
                    }
                }
            }

            Err(last_err)
        };

        match timeout {
            Some(d) => match tokio::time::timeout(d, inner_fut).await {
                Ok(result) => result,
                Err(_) => Err(Status::deadline_exceeded("RPC deadline exceeded")),
            },
            None => inner_fut.await,
        }
    }

    async fn call_unary_inner<Req, Resp>(
        &self,
        method: &str,
        req: &Req,
        metadata: &Metadata,
        timeout: Option<Duration>,
    ) -> Result<(Resp, Metadata), Status>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        // Encode the request with prost + 5-byte gRPC framing.
        let req_bytes = codec::encode_message(req, None)?;

        // Build request headers: user metadata + optional grpc-timeout.
        let mut header_map = metadata.to_header_map();
        if let Some(d) = timeout {
            let timeout_str = transport::encode_timeout(d);
            if let Ok(val) = timeout_str.parse::<http::header::HeaderValue>() {
                header_map.insert(
                    http::header::HeaderName::from_static("grpc-timeout"),
                    val,
                );
            }
        }

        // Build the terminal invoker: does the actual HTTP/2 round-trip.
        let transport = Arc::clone(&self.transport);
        let method_owned = method.to_owned();
        let header_map_arc = Arc::new(header_map);
        let terminal: interceptor::ClientNext = Arc::new(move |req_frame, _md| {
            let transport = Arc::clone(&transport);
            let method = method_owned.clone();
            let header_map = Arc::clone(&header_map_arc);
            Box::pin(async move {
                let mut stream = {
                    let mut guard = transport
                        .lock()
                        .map_err(|_| Status::internal("channel transport mutex poisoned"))?;
                    guard.new_stream(&method, &header_map)?
                };

                stream.send_message(req_frame, true)?;
                stream.recv_headers().await?;
                let resp_frame = stream.recv_message().await?;
                let (rpc_status, raw_trailers) = stream.recv_trailers().await?;
                if !rpc_status.is_ok() {
                    return Err(rpc_status);
                }
                let frame = resp_frame
                    .ok_or_else(|| Status::internal("server returned OK but sent no response body"))?;
                let trailing_metadata = Metadata::from_trailer_headers(&raw_trailers);
                Ok((frame, trailing_metadata))
            })
        });

        // Wrap terminal in the interceptor chain.
        let chain = interceptor::chain_client(&self.interceptors, method.to_owned(), terminal);

        // Invoke the chain.
        let (resp_frame, trailing_metadata) = chain(req_bytes, metadata.clone()).await?;

        // Decode response payload with prost.
        let resp: Resp = codec::decode_message(&resp_frame, None, codec::DEFAULT_MAX_RECV_SIZE)?;

        Ok((resp, trailing_metadata))
    }

    /// Open a streaming RPC call (server-streaming, client-streaming, or bidi).
    ///
    /// Returns a [`StreamCall`] that the caller uses to send/receive messages.
    /// The initial request HEADERS are sent immediately; the caller must send
    /// at least one message (or call [`StreamCall::close_send`]) before the server
    /// will respond.
    pub fn new_streaming_call(
        &self,
        method: &str,
        metadata: &Metadata,
        timeout: Option<Duration>,
    ) -> Result<StreamCall, Status> {
        let mut header_map = metadata.to_header_map();
        if let Some(d) = timeout {
            let timeout_str = transport::encode_timeout(d);
            if let Ok(val) = timeout_str.parse::<http::header::HeaderValue>() {
                header_map.insert(
                    http::header::HeaderName::from_static("grpc-timeout"),
                    val,
                );
            }
        }
        let stream = {
            let mut guard = self
                .transport
                .lock()
                .map_err(|_| Status::internal("channel transport mutex poisoned"))?;
            guard.new_stream(method, &header_map)?
        };
        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        Ok(StreamCall { stream, headers_received: false, deadline })
    }
}

// ── StreamCall ────────────────────────────────────────────────────────────────

/// A handle for a streaming gRPC call (server-streaming, client-streaming, or bidi).
///
/// Use [`send_message`](Self::send_message) to send request messages,
/// [`close_send`](Self::close_send) to signal end of the request stream,
/// and [`recv_message`](Self::recv_message) to receive response messages.
/// Call [`finish`](Self::finish) to get the final `(Status, trailing Metadata)`.
pub struct StreamCall {
    stream: crate::transport::ClientStream,
    headers_received: bool,
    /// Absolute deadline for all receive operations on this stream.
    deadline: Option<std::time::Instant>,
}

impl StreamCall {
    /// Encode and send one request message.
    pub fn send_message<Req: prost::Message>(&mut self, req: &Req) -> Result<(), Status> {
        let frame = codec::encode_message(req, None)?;
        self.stream.send_message(frame, false)
    }

    /// Close the send side (signals END_STREAM after the last request).
    pub fn close_send(&mut self) -> Result<(), Status> {
        self.stream.finish_send()
    }

    /// Cancel the call by sending RST_STREAM(CANCEL).
    ///
    /// After this, [`recv_message`](Self::recv_message) and [`finish`](Self::finish)
    /// will return `Status::cancelled`.
    pub fn cancel(&mut self) {
        self.stream.cancel();
    }

    /// Receive the next response message, if any.
    ///
    /// Automatically receives and validates initial response headers on the first call.
    /// Returns `Ok(None)` when the server has sent all messages and is about to send trailers.
    /// Returns `Err(Status::deadline_exceeded(...))` if the call deadline fires.
    pub async fn recv_message<Resp: prost::Message + Default>(
        &mut self,
    ) -> Result<Option<Resp>, Status> {
        let deadline = self.deadline;
        let inner = async {
            if !self.headers_received {
                self.stream.recv_headers().await?;
                self.headers_received = true;
            }
            match self.stream.recv_message().await? {
                Some(frame) => {
                    let msg = codec::decode_message(&frame, None, codec::DEFAULT_MAX_RECV_SIZE)?;
                    Ok(Some(msg))
                }
                None => Ok(None),
            }
        };
        apply_deadline(deadline, inner).await
    }

    /// Receive trailing metadata and the final gRPC status.
    ///
    /// Must be called after all messages have been received (i.e. after
    /// `recv_message` returned `Ok(None)`).
    pub async fn finish(&mut self) -> Result<Metadata, Status> {
        let deadline = self.deadline;
        let inner = async {
            if !self.headers_received {
                self.stream.recv_headers().await?;
                self.headers_received = true;
            }
            let (rpc_status, raw_trailers) = self.stream.recv_trailers().await?;
            if !rpc_status.is_ok() {
                return Err(rpc_status);
            }
            Ok(Metadata::from_trailer_headers(&raw_trailers))
        };
        apply_deadline(deadline, inner).await
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run `fut`, aborting with `DeadlineExceeded` if the absolute `deadline` fires first.
///
/// Also converts `CANCELLED` → `DEADLINE_EXCEEDED` when the deadline has passed:
/// the server may cancel the stream (RST_STREAM CANCEL) before our local timer fires,
/// but from the caller's perspective the cause is a deadline expiry.
async fn apply_deadline<T>(
    deadline: Option<std::time::Instant>,
    fut: impl std::future::Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    use crate::status::Code;
    if let Some(dl) = deadline {
        let remaining = dl
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(Duration::ZERO);
        let result = match tokio::time::timeout(remaining, fut).await {
            Ok(result) => result,
            Err(_) => return Err(Status::deadline_exceeded("streaming RPC deadline exceeded")),
        };
        // If the deadline has now elapsed and we received CANCELLED, translate to
        // DEADLINE_EXCEEDED.  This handles the race where the server sends
        // RST_STREAM(CANCEL) due to grpc-timeout expiry before our local timer fires.
        if let Err(ref e) = result {
            if e.code == Code::Cancelled && std::time::Instant::now() >= dl {
                return Err(Status::deadline_exceeded("streaming RPC deadline exceeded"));
            }
        }
        result
    } else {
        fut.await
    }
}

// ── Tests (Modules 4 + 5 + 6) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bytes::Bytes;
    use tokio::net::TcpListener;

    use crate::metadata::Metadata;
    use crate::retry::RetryPolicy;
    use crate::server::{Handler, MethodDesc, Server, ServiceDesc};
    use crate::status::Code;

    // ── Inline prost message definitions (no .proto file needed) ────────────

    #[derive(Clone, PartialEq, prost::Message)]
    struct HelloRequest {
        #[prost(string, tag = "1")]
        name: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct HelloReply {
        #[prost(string, tag = "1")]
        message: String,
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn start_server(desc: ServiceDesc) -> SocketAddr {
        let mut server = Server::new();
        server.add_service(desc);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        tokio::spawn(async move { server.serve(addr).await.ok(); });
        tokio::task::yield_now().await;
        addr
    }

    /// A handler that decodes `HelloRequest` and returns a `HelloReply`.
    fn greeter_handler() -> crate::server::UnaryHandlerFn {
        Arc::new(|req_bytes: Bytes, _md: Metadata| {
            Box::pin(async move {
                let req = HelloRequest::decode(req_bytes.as_ref())
                    .map_err(|e| Status::internal(format!("decode: {e}")))?;
                let reply = HelloReply {
                    message: format!("Hello, {}!", req.name),
                };
                let mut buf = Vec::new();
                use prost::Message as _;
                reply.encode(&mut buf)
                    .map_err(|e| Status::internal(format!("encode: {e}")))?;
                Ok(Bytes::from(buf))
            })
        })
    }

    // ── Module 5: Unary RPC end-to-end ──────────────────────────────────────

    #[tokio::test]
    async fn say_hello_end_to_end() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: Handler::Unary(greeter_handler()) }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let (reply, _): (HelloReply, _) = channel
            .call_unary("/helloworld.Greeter/SayHello", &HelloRequest { name: "World".into() }, &Metadata::new(), None)
            .await.unwrap();
        assert_eq!(reply.message, "Hello, World!");
    }

    #[tokio::test]
    async fn multiple_sequential_calls() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: Handler::Unary(greeter_handler()) }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        for name in &["Alice", "Bob", "Carol"] {
            let (reply, _): (HelloReply, _) = channel
                .call_unary("/helloworld.Greeter/SayHello", &HelloRequest { name: name.to_string() }, &Metadata::new(), None)
                .await.unwrap();
            assert_eq!(reply.message, format!("Hello, {name}!"));
        }
    }

    #[tokio::test]
    async fn server_error_propagates_through_channel() {
        let addr = start_server(ServiceDesc {
            name: "test.Fail",
            methods: vec![MethodDesc {
                name: "Fail",
                handler: Handler::Unary(Arc::new(|_: Bytes, _md: Metadata| {
                    Box::pin(async move { Err(Status::not_found("resource missing")) })
                })),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let result: Result<(HelloReply, Metadata), Status> = channel
            .call_unary("/test.Fail/Fail", &HelloRequest::default(), &Metadata::new(), None)
            .await;
        let err = result.unwrap_err();
        assert_eq!(err.code, Code::NotFound);
        assert_eq!(err.message, "resource missing");
    }

    #[tokio::test]
    async fn unknown_method_via_channel() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: Handler::Unary(greeter_handler()) }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let result: Result<(HelloReply, Metadata), Status> = channel
            .call_unary("/helloworld.Greeter/SayGoodbye", &HelloRequest::default(), &Metadata::new(), None)
            .await;
        assert_eq!(result.unwrap_err().code, Code::Unimplemented);
    }

    // ── Module 6: Metadata round-trip ────────────────────────────────────────

    /// Client sends request metadata; server reads it and echoes the value back
    /// in the response body.
    #[tokio::test]
    async fn request_metadata_reaches_handler() {
        let addr = start_server(ServiceDesc {
            name: "test.Meta",
            methods: vec![MethodDesc {
                name: "Echo",
                // Echo back the x-client-id metadata value as the response body
                handler: Handler::Unary(Arc::new(|_req: Bytes, md: Metadata| {
                    Box::pin(async move {
                        let id = md.get("x-client-id").unwrap_or("(none)").to_owned();
                        Ok(Bytes::from(id.into_bytes()))
                    })
                })),
            }],
        }).await;

        let _channel = Channel::connect(addr).await.unwrap();

        let mut req_meta = Metadata::new();
        req_meta.insert("x-client-id", "test-client-42");

        // Use a plain-bytes RPC: server returns raw bytes; we decode as string.
        // We'll use HelloRequest as a zero-bytes dummy and interpret the "reply" as raw.
        // Better: use a custom approach at the transport level.
        // For this test, use the raw ClientTransport path so we can verify raw bytes.
        use crate::transport::ClientTransport;
        use crate::codec;
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut transport, conn_fut) = ClientTransport::connect(tcp).await.unwrap();
        tokio::spawn(conn_fut);

        let header_map = req_meta.to_header_map();
        let mut stream = transport.new_stream("/test.Meta/Echo", &header_map).unwrap();
        stream.send_message(codec::encode_raw(b"unused", None).unwrap(), true).unwrap();
        stream.recv_headers().await.unwrap();
        let frame = stream.recv_message().await.unwrap().unwrap();
        let payload = codec::decode_raw(&frame, None, codec::DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(payload, b"test-client-42");

        let (status, _) = stream.recv_trailers().await.unwrap();
        assert!(status.is_ok());
    }

    // ── Module 7: Deadline / cancellation ────────────────────────────────────

    /// A call with a very short timeout against a slow handler returns DeadlineExceeded.
    #[tokio::test]
    async fn deadline_exceeded_on_slow_handler() {
        use std::time::Duration;

        let addr = start_server(ServiceDesc {
            name: "test.Slow",
            methods: vec![MethodDesc {
                name: "Slow",
                handler: Handler::Unary(Arc::new(|_: Bytes, _md: Metadata| {
                    Box::pin(async move {
                        // Sleep much longer than the client timeout.
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        Ok(Bytes::new())
                    })
                })),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let result: Result<(HelloReply, Metadata), Status> = channel
            .call_unary(
                "/test.Slow/Slow",
                &HelloRequest::default(),
                &Metadata::new(),
                Some(Duration::from_millis(50)),
            )
            .await;
        assert_eq!(result.unwrap_err().code, crate::status::Code::DeadlineExceeded);
    }

    /// A call with a generous timeout completes normally.
    #[tokio::test]
    async fn call_with_timeout_succeeds_before_deadline() {
        use std::time::Duration;

        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: Handler::Unary(greeter_handler()) }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let (reply, _): (HelloReply, _) = channel
            .call_unary(
                "/helloworld.Greeter/SayHello",
                &HelloRequest { name: "Timeout".into() },
                &Metadata::new(),
                Some(Duration::from_secs(5)),
            )
            .await
            .unwrap();
        assert_eq!(reply.message, "Hello, Timeout!");
    }

    // ── Module 8: Streaming RPCs ──────────────────────────────────────────────

    /// Server-streaming: client sends one request, server sends N replies.
    #[tokio::test]
    async fn server_streaming_multiple_responses() {
        use crate::server::StreamingHandlerFn;
        use crate::transport::ServerStream;

        // Handler: read one request, send 3 replies, then close.
        let handler: StreamingHandlerFn = Arc::new(|mut stream: ServerStream, _md: Metadata| {
            Box::pin(async move {
                // Read (and discard) the single request.
                let _ = stream.recv_message().await;
                stream.send_headers(&http::HeaderMap::new()).unwrap();
                for i in 1u32..=3 {
                    use prost::Message as _;
                    let reply = HelloReply { message: format!("reply {i}") };
                    let mut buf = Vec::new();
                    reply.encode(&mut buf).unwrap();
                    let frame = crate::codec::encode_raw(&buf, None).unwrap();
                    stream.send_message(frame).unwrap();
                }
                stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
            })
        });

        let addr = start_server(ServiceDesc {
            name: "test.Streaming",
            methods: vec![MethodDesc {
                name: "ServerStream",
                handler: Handler::Streaming(handler),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let mut call = channel
            .new_streaming_call("/test.Streaming/ServerStream", &Metadata::new(), None)
            .unwrap();

        // Send one request then close send side.
        call.send_message(&HelloRequest { name: "go".into() }).unwrap();
        call.close_send().unwrap();

        // Collect all responses.
        let mut replies = Vec::new();
        while let Some(reply) = call.recv_message::<HelloReply>().await.unwrap() {
            replies.push(reply.message.clone());
        }
        call.finish().await.unwrap();

        assert_eq!(replies, vec!["reply 1", "reply 2", "reply 3"]);
    }

    /// Client-streaming: client sends N requests, server sends one reply.
    #[tokio::test]
    async fn client_streaming_accumulate_names() {
        use crate::server::StreamingHandlerFn;
        use crate::transport::ServerStream;

        // Handler: read all requests, concatenate names, send one reply.
        let handler: StreamingHandlerFn = Arc::new(|mut stream: ServerStream, _md: Metadata| {
            Box::pin(async move {
                let mut names = Vec::new();
                while let Ok(Some(frame)) = stream.recv_message().await {
                    use prost::Message as _;
                    if let Ok(req) = HelloRequest::decode(&frame[5..]) {
                        names.push(req.name);
                    }
                }
                stream.send_headers(&http::HeaderMap::new()).unwrap();
                use prost::Message as _;
                let reply = HelloReply { message: format!("Hello, {}!", names.join(", ")) };
                let mut buf = Vec::new();
                reply.encode(&mut buf).unwrap();
                let frame = crate::codec::encode_raw(&buf, None).unwrap();
                stream.send_message(frame).unwrap();
                stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
            })
        });

        let addr = start_server(ServiceDesc {
            name: "test.ClientStream",
            methods: vec![MethodDesc {
                name: "Accumulate",
                handler: Handler::Streaming(handler),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let mut call = channel
            .new_streaming_call("/test.ClientStream/Accumulate", &Metadata::new(), None)
            .unwrap();

        for name in &["Alice", "Bob", "Carol"] {
            call.send_message(&HelloRequest { name: name.to_string() }).unwrap();
        }
        call.close_send().unwrap();

        let reply: HelloReply = call.recv_message().await.unwrap().unwrap();
        call.finish().await.unwrap();

        assert_eq!(reply.message, "Hello, Alice, Bob, Carol!");
    }

    /// Bidirectional streaming: echo each message back.
    #[tokio::test]
    async fn bidi_streaming_echo() {
        use crate::server::StreamingHandlerFn;
        use crate::transport::ServerStream;

        let handler: StreamingHandlerFn = Arc::new(|mut stream: ServerStream, _md: Metadata| {
            Box::pin(async move {
                stream.send_headers(&http::HeaderMap::new()).unwrap();
                while let Ok(Some(frame)) = stream.recv_message().await {
                    use prost::Message as _;
                    if let Ok(req) = HelloRequest::decode(&frame[5..]) {
                        let reply = HelloReply { message: format!("echo: {}", req.name) };
                        let mut buf = Vec::new();
                        reply.encode(&mut buf).unwrap();
                        let resp_frame = crate::codec::encode_raw(&buf, None).unwrap();
                        if stream.send_message(resp_frame).is_err() {
                            break;
                        }
                    }
                }
                stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
            })
        });

        let addr = start_server(ServiceDesc {
            name: "test.Bidi",
            methods: vec![MethodDesc {
                name: "Echo",
                handler: Handler::Streaming(handler),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let mut call = channel
            .new_streaming_call("/test.Bidi/Echo", &Metadata::new(), None)
            .unwrap();

        for name in &["X", "Y", "Z"] {
            call.send_message(&HelloRequest { name: name.to_string() }).unwrap();
            let reply: HelloReply = call.recv_message().await.unwrap().unwrap();
            assert_eq!(reply.message, format!("echo: {name}"));
        }
        call.close_send().unwrap();

        // After close_send, server should exit its loop and send trailers.
        assert!(call.recv_message::<HelloReply>().await.unwrap().is_none());
        call.finish().await.unwrap();
    }

    // ── Module 10: Interceptors ───────────────────────────────────────────────

    /// Client interceptor that adds a metadata header to every request.
    #[tokio::test]
    async fn client_interceptor_adds_metadata() {
        use crate::interceptor::UnaryClientInterceptor;
        use std::sync::{Arc, Mutex};

        // Track which metadata the interceptor saw.
        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_clone = Arc::clone(&seen);

        let interceptor: UnaryClientInterceptor = Arc::new(move |_method, req, mut md, next| {
            let seen_clone = Arc::clone(&seen_clone);
            Box::pin(async move {
                md.insert("x-intercepted", "client-yes");
                let result = next(req, md).await;
                *seen_clone.lock().unwrap() = Some("ran".to_owned());
                result
            })
        });

        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: Handler::Unary(greeter_handler()) }],
        }).await;

        let mut channel = Channel::connect(addr).await.unwrap();
        channel.add_interceptor(interceptor);

        let (reply, _): (HelloReply, _) = channel
            .call_unary("/helloworld.Greeter/SayHello", &HelloRequest { name: "Intercepted".into() }, &Metadata::new(), None)
            .await.unwrap();

        assert_eq!(reply.message, "Hello, Intercepted!");
        assert_eq!(seen.lock().unwrap().as_deref(), Some("ran"));
    }

    /// Server interceptor that injects metadata and can short-circuit.
    #[tokio::test]
    async fn server_interceptor_short_circuits() {
        use crate::interceptor::UnaryServerInterceptor;

        // Interceptor that always returns PermissionDenied.
        let interceptor: UnaryServerInterceptor = Arc::new(
            |_method, _req, _md, _next| {
                Box::pin(async move { Err(Status::new(crate::status::Code::PermissionDenied, "blocked")) })
            }
        );

        let _addr = start_server(ServiceDesc {
            name: "test.Blocked",
            methods: vec![MethodDesc {
                name: "Call",
                handler: Handler::Unary(Arc::new(|_: Bytes, _md: Metadata| {
                    Box::pin(async move { panic!("handler must not run") })
                })),
            }],
        }).await;

        // We need to add interceptors to a server, but `start_server` uses the simple helper.
        // Test using our own server setup with an interceptor.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener.local_addr().unwrap();
        drop(listener);

        let mut server = crate::server::Server::new();
        server.add_service(ServiceDesc {
            name: "test.Guarded",
            methods: vec![MethodDesc {
                name: "Guarded",
                handler: Handler::Unary(Arc::new(|_: Bytes, _md: Metadata| {
                    Box::pin(async move { panic!("must not reach handler") })
                })),
            }],
        });
        server.add_interceptor(interceptor);
        tokio::spawn(async move { server.serve(addr2).await.ok(); });
        tokio::task::yield_now().await;

        let channel = Channel::connect(addr2).await.unwrap();
        let result: Result<(HelloReply, Metadata), Status> = channel
            .call_unary("/test.Guarded/Guarded", &HelloRequest::default(), &Metadata::new(), None)
            .await;

        let err = result.unwrap_err();
        assert_eq!(err.code, crate::status::Code::PermissionDenied);
    }

    // ── Module 12: Retry policy ──────────────────────────────────────────────

    /// A handler that fails the first N calls with `Unavailable`, then succeeds.
    fn flaky_handler(fail_times: usize) -> crate::server::UnaryHandlerFn {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        Arc::new(move |_req: Bytes, _md: Metadata| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < fail_times {
                    Err(Status::unavailable("temporary failure"))
                } else {
                    // HelloReply { message: "" } encodes to zero bytes in protobuf.
                    Ok(Bytes::new())
                }
            })
        })
    }

    #[tokio::test]
    async fn retry_succeeds_on_second_attempt() {
        let addr = start_server(ServiceDesc {
            name: "test.Retry",
            methods: vec![MethodDesc {
                name: "Call",
                handler: Handler::Unary(flaky_handler(1)),
            }],
        }).await;

        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            retryable_codes: vec![Code::Unavailable],
        };
        let channel = Channel::connect(addr).await.unwrap().with_retry_policy(policy);
        let (reply, _): (HelloReply, _) = channel
            .call_unary("/test.Retry/Call", &HelloRequest::default(), &Metadata::new(), None)
            .await
            .expect("should succeed after retry");
        let _ = reply;
    }

    #[tokio::test]
    async fn retry_exhausted_returns_last_error() {
        let addr = start_server(ServiceDesc {
            name: "test.RetryExhaust",
            methods: vec![MethodDesc {
                name: "Call",
                handler: Handler::Unary(flaky_handler(10)), // always fails
            }],
        }).await;

        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            retryable_codes: vec![Code::Unavailable],
        };
        let channel = Channel::connect(addr).await.unwrap().with_retry_policy(policy);
        let err = channel
            .call_unary::<HelloRequest, HelloReply>("/test.RetryExhaust/Call", &HelloRequest::default(), &Metadata::new(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, Code::Unavailable);
    }

    #[tokio::test]
    async fn non_retryable_code_not_retried() {
        // Handler always returns PermissionDenied; policy only retries Unavailable.
        let addr = start_server(ServiceDesc {
            name: "test.NoRetry",
            methods: vec![MethodDesc {
                name: "Call",
                handler: Handler::Unary(Arc::new(|_: Bytes, _: Metadata| {
                    Box::pin(async { Err(Status::new(Code::PermissionDenied, "denied")) })
                })),
            }],
        }).await;

        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            retryable_codes: vec![Code::Unavailable],
        };
        let channel = Channel::connect(addr).await.unwrap().with_retry_policy(policy);
        let err = channel
            .call_unary::<HelloRequest, HelloReply>("/test.NoRetry/Call", &HelloRequest::default(), &Metadata::new(), None)
            .await
            .unwrap_err();
        // Should fail immediately without exhausting all 5 attempts.
        assert_eq!(err.code, Code::PermissionDenied);
    }
}
