//! Helloworld gRPC server example.
//!
//! Mirrors the canonical grpc-go helloworld server.
//! Listens on port 50051 and implements `helloworld.Greeter/SayHello`.
//!
//! Run with:
//!   cargo run --example helloworld_server
//!
//! Set RUST_LOG=debug to see gRPC traces.

use prost::Message;

use grpc_rs::server::{unary_handler, Handler, MethodDesc, Server, ServiceDesc};

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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut server = Server::new();
    server.add_service(ServiceDesc {
        name: "helloworld.Greeter",
        methods: vec![MethodDesc {
            name: "SayHello",
            handler: Handler::Unary(unary_handler(|req: HelloRequest, _md| async move {
                tracing::info!(name = %req.name, "SayHello");
                Ok(HelloReply {
                    message: format!("Hello, {}!", req.name),
                })
            })),
        }],
    });

    let addr = "0.0.0.0:50051";
    println!("gRPC server listening on {addr}");
    server
        .serve(addr.parse().unwrap())
        .await
        .expect("server failed");
}
