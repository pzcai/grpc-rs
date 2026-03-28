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

use http::HeaderMap;
use tokio::net::TcpStream;

use crate::codec;
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
    /// - `method`: the full method path, e.g. `"/helloworld.Greeter/SayHello"`.
    /// - `req`: the request message (serialized with prost).
    /// - `metadata`: optional request headers.
    ///
    /// Returns the deserialized response, or a [`Status`] error if the server
    /// returns a non-OK gRPC status or if any transport/codec error occurs.
    pub async fn call_unary<Req, Resp>(
        &self,
        method: &str,
        req: &Req,
        metadata: &HeaderMap,
    ) -> Result<Resp, Status>
    where
        Req: prost::Message,
        Resp: prost::Message + Default,
    {
        // Encode the request with prost + 5-byte gRPC framing.
        let req_frame = codec::encode_message(req, None)?;

        // Acquire the transport lock only for the synchronous new_stream call.
        let mut stream = {
            let mut guard = self
                .transport
                .lock()
                .map_err(|_| Status::internal("channel transport mutex poisoned"))?;
            guard.new_stream(method, metadata)?
        };

        // Send request (END_STREAM on the only message — this is a unary call).
        stream.send_message(req_frame, true)?;

        // Receive response headers (blocks until server sends initial HEADERS).
        stream.recv_headers().await?;

        // Receive the response data frame (None for trailer-only responses, e.g. errors).
        let resp_frame = stream.recv_message().await?;

        // Receive trailing HEADERS — always do this even if there was no data frame,
        // so we surface the real gRPC status code (e.g. NotFound, Unimplemented).
        let (rpc_status, _trailers) = stream.recv_trailers().await?;
        if !rpc_status.is_ok() {
            return Err(rpc_status);
        }

        // Status is OK — there must be a response frame to decode.
        let frame = resp_frame
            .ok_or_else(|| Status::internal("server returned OK but sent no response body"))?;

        // Decode response payload with prost.
        let resp: Resp = codec::decode_message(&frame, None, codec::DEFAULT_MAX_RECV_SIZE)?;

        Ok(resp)
    }
}

// ── Tests (Modules 4 + 5) ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bytes::Bytes;
    use tokio::net::TcpListener;

    use crate::server::{MethodDesc, Server, ServiceDesc};
    use crate::status::Code;

    // ── Inline prost message definitions (no .proto file needed) ────────────

    /// Greeter request: contains the name to greet.
    #[derive(Clone, PartialEq, prost::Message)]
    struct HelloRequest {
        #[prost(string, tag = "1")]
        name: String,
    }

    /// Greeter response: contains the greeting message.
    #[derive(Clone, PartialEq, prost::Message)]
    struct HelloReply {
        #[prost(string, tag = "1")]
        message: String,
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Build and start a server, returning the bound address.
    async fn start_server(desc: ServiceDesc) -> SocketAddr {
        let mut server = Server::new();
        server.add_service(desc);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        tokio::spawn(async move {
            server.serve(addr).await.ok();
        });
        tokio::task::yield_now().await;
        addr
    }

    /// A handler that decodes `HelloRequest` and returns a `HelloReply`.
    fn greeter_handler() -> crate::server::UnaryHandlerFn {
        Arc::new(|req_bytes: Bytes| {
            Box::pin(async move {
                let req = HelloRequest::decode(req_bytes.as_ref())
                    .map_err(|e| Status::internal(format!("decode HelloRequest: {e}")))?;

                let reply = HelloReply {
                    message: format!("Hello, {}!", req.name),
                };

                let mut buf = Vec::new();
                use prost::Message as _;
                reply
                    .encode(&mut buf)
                    .map_err(|e| Status::internal(format!("encode HelloReply: {e}")))?;
                Ok(Bytes::from(buf))
            })
        })
    }

    // ── Module 5: Unary RPC end-to-end ──────────────────────────────────────

    /// Full unary RPC with prost messages: Greeter.SayHello end-to-end.
    #[tokio::test]
    async fn say_hello_end_to_end() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc {
                name: "SayHello",
                handler: greeter_handler(),
            }],
        })
        .await;

        let channel = Channel::connect(addr).await.unwrap();

        let request = HelloRequest {
            name: "World".to_string(),
        };
        let reply: HelloReply = channel
            .call_unary("/helloworld.Greeter/SayHello", &request, &HeaderMap::new())
            .await
            .unwrap();

        assert_eq!(reply.message, "Hello, World!");
    }

    /// Multiple sequential calls on the same Channel.
    #[tokio::test]
    async fn multiple_sequential_calls() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc {
                name: "SayHello",
                handler: greeter_handler(),
            }],
        })
        .await;

        let channel = Channel::connect(addr).await.unwrap();

        for name in &["Alice", "Bob", "Carol"] {
            let req = HelloRequest { name: name.to_string() };
            let reply: HelloReply = channel
                .call_unary("/helloworld.Greeter/SayHello", &req, &HeaderMap::new())
                .await
                .unwrap();
            assert_eq!(reply.message, format!("Hello, {name}!"));
        }
    }

    /// Server error propagates through Channel::call_unary.
    #[tokio::test]
    async fn server_error_propagates_through_channel() {
        let addr = start_server(ServiceDesc {
            name: "test.Fail",
            methods: vec![MethodDesc {
                name: "Fail",
                handler: Arc::new(|_: Bytes| {
                    Box::pin(async move {
                        Err(Status::not_found("resource missing"))
                    })
                }),
            }],
        })
        .await;

        let channel = Channel::connect(addr).await.unwrap();

        // We still need a prost message type; use HelloRequest as a dummy.
        let result: Result<HelloReply, Status> = channel
            .call_unary("/test.Fail/Fail", &HelloRequest::default(), &HeaderMap::new())
            .await;

        let err = result.unwrap_err();
        assert_eq!(err.code, Code::NotFound);
        assert_eq!(err.message, "resource missing");
    }

    /// Calling an unknown method returns Unimplemented.
    #[tokio::test]
    async fn unknown_method_via_channel() {
        let addr = start_server(ServiceDesc {
            name: "helloworld.Greeter",
            methods: vec![MethodDesc {
                name: "SayHello",
                handler: greeter_handler(),
            }],
        })
        .await;

        let channel = Channel::connect(addr).await.unwrap();

        let result: Result<HelloReply, Status> = channel
            .call_unary(
                "/helloworld.Greeter/SayGoodbye",
                &HelloRequest::default(),
                &HeaderMap::new(),
            )
            .await;

        assert_eq!(result.unwrap_err().code, Code::Unimplemented);
    }
}
