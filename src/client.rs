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
use std::sync::Mutex;

use tokio::net::TcpStream;

use crate::codec;
use crate::metadata::Metadata;
use crate::status::Status;
use crate::transport::ClientTransport;

/// A gRPC channel: a logical connection to a single server endpoint.
///
/// Cheaply cloneable in the future (when backed by a connection pool); for now
/// it holds one HTTP/2 connection.
pub struct Channel {
    transport: Mutex<ClientTransport>,
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
            transport: Mutex::new(transport),
        })
    }

    /// Make a unary RPC call.
    ///
    /// Make a unary RPC call.
    ///
    /// - `method`: the full method path, e.g. `"/helloworld.Greeter/SayHello"`.
    /// - `req`: the request message (serialized with prost).
    /// - `metadata`: request metadata (user-defined headers).
    ///
    /// Returns `(response, trailing_metadata)` on success, or a [`Status`] error.
    /// The trailing metadata contains any user-defined headers the server sent in
    /// its trailing HEADERS frame (excludes `grpc-status` and `grpc-message`).
    pub async fn call_unary<Req, Resp>(
        &self,
        method: &str,
        req: &Req,
        metadata: &Metadata,
    ) -> Result<(Resp, Metadata), Status>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        // Encode the request with prost + 5-byte gRPC framing.
        let req_frame = codec::encode_message(req, None)?;

        // Convert Metadata to HeaderMap for the h2 request headers.
        let header_map = metadata.to_header_map();

        // Acquire the transport lock only for the synchronous new_stream call.
        let mut stream = {
            let mut guard = self
                .transport
                .lock()
                .map_err(|_| Status::internal("channel transport mutex poisoned"))?;
            guard.new_stream(method, &header_map)?
        };

        // Send request (END_STREAM on the only message — this is a unary call).
        stream.send_message(req_frame, true)?;

        // Receive response headers (blocks until server sends initial HEADERS).
        stream.recv_headers().await?;

        // Receive the response data frame (None for trailer-only responses, e.g. errors).
        let resp_frame = stream.recv_message().await?;

        // Receive trailing HEADERS — always do this even if there was no data frame,
        // so we surface the real gRPC status code (e.g. NotFound, Unimplemented).
        let (rpc_status, raw_trailers) = stream.recv_trailers().await?;
        if !rpc_status.is_ok() {
            return Err(rpc_status);
        }

        // Status is OK — there must be a response frame to decode.
        let frame = resp_frame
            .ok_or_else(|| Status::internal("server returned OK but sent no response body"))?;

        // Decode response payload with prost.
        let resp: Resp = codec::decode_message(&frame, None, codec::DEFAULT_MAX_RECV_SIZE)?;

        // Extract user-defined trailing metadata (strips grpc-status, grpc-message).
        let trailing_metadata = Metadata::from_trailer_headers(&raw_trailers);

        Ok((resp, trailing_metadata))
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
    use crate::server::{MethodDesc, Server, ServiceDesc};
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
            methods: vec![MethodDesc { name: "SayHello", handler: greeter_handler() }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let (reply, _): (HelloReply, _) = channel
            .call_unary("/helloworld.Greeter/SayHello", &HelloRequest { name: "World".into() }, &Metadata::new())
            .await.unwrap();
        assert_eq!(reply.message, "Hello, World!");
    }

    #[tokio::test]
    async fn multiple_sequential_calls() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: greeter_handler() }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        for name in &["Alice", "Bob", "Carol"] {
            let (reply, _): (HelloReply, _) = channel
                .call_unary("/helloworld.Greeter/SayHello", &HelloRequest { name: name.to_string() }, &Metadata::new())
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
                handler: Arc::new(|_: Bytes, _md: Metadata| {
                    Box::pin(async move { Err(Status::not_found("resource missing")) })
                }),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let result: Result<(HelloReply, Metadata), Status> = channel
            .call_unary("/test.Fail/Fail", &HelloRequest::default(), &Metadata::new())
            .await;
        let err = result.unwrap_err();
        assert_eq!(err.code, Code::NotFound);
        assert_eq!(err.message, "resource missing");
    }

    #[tokio::test]
    async fn unknown_method_via_channel() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc { name: "SayHello", handler: greeter_handler() }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();
        let result: Result<(HelloReply, Metadata), Status> = channel
            .call_unary("/helloworld.Greeter/SayGoodbye", &HelloRequest::default(), &Metadata::new())
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
                handler: Arc::new(|_req: Bytes, md: Metadata| {
                    Box::pin(async move {
                        let id = md.get("x-client-id").unwrap_or("(none)").to_owned();
                        Ok(Bytes::from(id.into_bytes()))
                    })
                }),
            }],
        }).await;

        let channel = Channel::connect(addr).await.unwrap();

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
}
