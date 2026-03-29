//! Helloworld gRPC client example.
//!
//! Mirrors the canonical grpc-go helloworld client.
//! Connects to localhost:50051 and calls `helloworld.Greeter/SayHello`.
//!
//! Run with (after starting helloworld_server):
//!   cargo run --example helloworld_client
//!
//! Optionally pass a name:
//!   cargo run --example helloworld_client -- World

use std::net::SocketAddr;

use prost::Message;

use grpc_rs::client::Channel;
use grpc_rs::metadata::Metadata;

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

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "World".to_string());

    let addr: SocketAddr = "127.0.0.1:50051".parse().unwrap();
    let channel = Channel::connect(addr)
        .await
        .expect("failed to connect to server");

    let req = HelloRequest { name: name.clone() };
    let (reply, _trailing): (HelloReply, _) = channel
        .call_unary(
            "/helloworld.Greeter/SayHello",
            &req,
            &Metadata::new(),
            None,
        )
        .await
        .expect("RPC failed");

    println!("Greeting: {}", reply.message);
}
