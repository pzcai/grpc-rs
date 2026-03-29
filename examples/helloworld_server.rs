//! Helloworld gRPC server example.
//!
//! Mirrors the canonical grpc-go helloworld server.
//! Listens on port 50051 and implements `helloworld.Greeter/SayHello`.
//!
//! Run with:
//!   cargo run --example helloworld_server

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use prost::Message;

use grpc_rs::metadata::Metadata;
use grpc_rs::server::{BoxFuture, Handler, MethodDesc, Server, ServiceDesc, UnaryHandlerFn};
use grpc_rs::status::Status;

// ── Protobuf message definitions (mirrors helloworld.proto) ──────────────────

#[derive(Clone, PartialEq, Message)]
struct HelloRequest {
    #[prost(string, tag = "1")]
    name: String,
}

#[derive(Clone, PartialEq, Message)]
struct HelloReply {
    #[prost(string, tag = "1")]
    message: String,
}

// ── Service implementation ────────────────────────────────────────────────────

fn say_hello_handler() -> UnaryHandlerFn {
    Arc::new(|req_bytes: Bytes, _md: Metadata| -> BoxFuture<Result<Bytes, Status>> {
        Box::pin(async move {
            let req = HelloRequest::decode(req_bytes.as_ref())
                .map_err(|e| Status::internal(format!("decode request: {e}")))?;

            println!("Received: name = {:?}", req.name);

            let reply = HelloReply {
                message: format!("Hello, {}!", req.name),
            };
            let mut buf = Vec::new();
            reply
                .encode(&mut buf)
                .map_err(|e| Status::internal(format!("encode reply: {e}")))?;
            Ok(Bytes::from(buf))
        })
    })
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let addr: SocketAddr = "0.0.0.0:50051".parse().unwrap();

    let mut server = Server::new();
    server.add_service(ServiceDesc {
        name: "helloworld.Greeter",
        methods: vec![MethodDesc {
            name: "SayHello",
            handler: Handler::Unary(say_hello_handler()),
        }],
    });

    println!("gRPC server listening on {addr}");
    server.serve(addr).await.expect("server failed");
}
