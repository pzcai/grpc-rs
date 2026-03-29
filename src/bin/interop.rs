//! gRPC interop test binary.
//!
//! Implements the gRPC interop test cases described at:
//! https://github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md
//!
//! Usage:
//!   # Run interop client against a server (e.g. grpc-go reference):
//!   cargo run --bin interop -- --mode=client --server_host=localhost --server_port=10000 \
//!       --test_case=empty_unary
//!
//!   # Run interop server (for reference clients to test against):
//!   cargo run --bin interop -- --mode=server --port=10000
//!
//! Supported test cases:
//!   empty_unary, large_unary, client_streaming, server_streaming,
//!   ping_pong, empty_stream

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use prost::Message;

use grpc_rs::client::{Channel, StreamCall};
use grpc_rs::metadata::Metadata;
use grpc_rs::server::{BoxFuture, Handler, MethodDesc, Server, ServiceDesc, StreamingHandlerFn, UnaryHandlerFn};
use grpc_rs::status::Status;
use grpc_rs::transport::ServerStream;

// ── Protobuf message definitions ─────────────────────────────────────────────
// Inline versions of the gRPC testing proto messages, matching the field
// numbers from grpc/testing/test.proto.

#[derive(Clone, PartialEq, Message)]
struct Empty {}

#[derive(Clone, PartialEq, Message)]
struct Payload {
    /// 0 = COMPRESSABLE, 1 = UNCOMPRESSABLE, 2 = RANDOM
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(bytes = "bytes", tag = "2")]
    body: Bytes,
}

#[derive(Clone, PartialEq, Message)]
struct SimpleRequest {
    /// Desired payload type in the response from the server. (0 = COMPRESSABLE)
    #[prost(int32, tag = "1")]
    response_type: i32,
    /// Desired payload size in the response from the server.
    #[prost(int32, tag = "2")]
    response_size: i32,
    /// Optional input payload sent along with the request.
    #[prost(message, optional, tag = "3")]
    payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct SimpleResponse {
    #[prost(message, optional, tag = "1")]
    payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingInputCallRequest {
    #[prost(message, optional, tag = "1")]
    payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingInputCallResponse {
    #[prost(int32, tag = "1")]
    aggregated_payload_size: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ResponseParameters {
    #[prost(int32, tag = "1")]
    size: i32,
    #[prost(int32, tag = "2")]
    interval_us: i32,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingOutputCallRequest {
    #[prost(int32, tag = "1")]
    response_type: i32,
    #[prost(message, repeated, tag = "2")]
    response_parameters: Vec<ResponseParameters>,
    #[prost(message, optional, tag = "3")]
    payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingOutputCallResponse {
    #[prost(message, optional, tag = "1")]
    payload: Option<Payload>,
}

// ── CLI argument parsing ──────────────────────────────────────────────────────

struct Args {
    mode: String,          // "client" or "server"
    server_host: String,
    server_port: u16,
    port: u16,
    test_case: String,
}

fn parse_args() -> Args {
    let mut mode = "client".to_owned();
    let mut server_host = "localhost".to_owned();
    let mut server_port: u16 = 10000;
    let mut port: u16 = 10000;
    let mut test_case = "empty_unary".to_owned();

    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--mode=") {
            mode = v.to_owned();
        } else if let Some(v) = arg.strip_prefix("--server_host=") {
            server_host = v.to_owned();
        } else if let Some(v) = arg.strip_prefix("--server_port=") {
            server_port = v.parse().expect("invalid server_port");
        } else if let Some(v) = arg.strip_prefix("--port=") {
            port = v.parse().expect("invalid port");
        } else if let Some(v) = arg.strip_prefix("--test_case=") {
            test_case = v.to_owned();
        }
    }

    Args { mode, server_host, server_port, port, test_case }
}

// ── Interop client test cases ─────────────────────────────────────────────────

/// empty_unary: client sends Empty, server returns Empty.
async fn test_empty_unary(ch: &Channel) {
    let (_resp, _): (Empty, _) = ch
        .call_unary("/grpc.testing.TestService/EmptyCall", &Empty {}, &Metadata::new(), None)
        .await
        .expect("empty_unary failed");
    println!("empty_unary: PASS");
}

/// large_unary: client sends a 271828-byte payload, expects a 314159-byte response.
async fn test_large_unary(ch: &Channel) {
    let req = SimpleRequest {
        response_type: 0,
        response_size: 314159,
        payload: Some(Payload {
            r#type: 0,
            body: Bytes::from(vec![0u8; 271828]),
        }),
    };
    let (resp, _): (SimpleResponse, _) = ch
        .call_unary("/grpc.testing.TestService/UnaryCall", &req, &Metadata::new(), None)
        .await
        .expect("large_unary failed");
    let size = resp.payload.map(|p| p.body.len()).unwrap_or(0);
    assert_eq!(size, 314159, "large_unary: wrong response size");
    println!("large_unary: PASS");
}

/// client_streaming: client sends 4 payloads (27182, 8, 1828, 45904 bytes),
/// expects aggregated_payload_size = 74922.
async fn test_client_streaming(ch: &Channel) {
    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/StreamingInputCall", &Metadata::new(), None)
        .expect("open stream failed");

    for size in &[27182usize, 8, 1828, 45904] {
        let req = StreamingInputCallRequest {
            payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; *size]) }),
        };
        call.send_message(&req).expect("send_message failed");
    }
    call.close_send().expect("close_send failed");

    let resp: StreamingInputCallResponse = call.recv_message().await.expect("recv failed").expect("no response");
    call.finish().await.expect("finish failed");

    assert_eq!(resp.aggregated_payload_size, 74922, "client_streaming: wrong size");
    println!("client_streaming: PASS");
}

/// server_streaming: server sends 4 responses (31415, 9, 2653, 58979 bytes).
async fn test_server_streaming(ch: &Channel) {
    let req = StreamingOutputCallRequest {
        response_type: 0,
        response_parameters: vec![
            ResponseParameters { size: 31415, interval_us: 0 },
            ResponseParameters { size: 9,     interval_us: 0 },
            ResponseParameters { size: 2653,  interval_us: 0 },
            ResponseParameters { size: 58979, interval_us: 0 },
        ],
        payload: None,
    };

    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/StreamingOutputCall", &Metadata::new(), None)
        .expect("open stream failed");
    call.send_message(&req).expect("send_message failed");
    call.close_send().expect("close_send failed");

    let expected_sizes = [31415usize, 9, 2653, 58979];
    for expected in expected_sizes {
        let resp: StreamingOutputCallResponse =
            call.recv_message().await.expect("recv failed").expect("unexpected end of stream");
        let actual = resp.payload.map(|p| p.body.len()).unwrap_or(0);
        assert_eq!(actual, expected, "server_streaming: wrong response size");
    }
    assert!(call.recv_message::<StreamingOutputCallResponse>().await.expect("recv failed").is_none());
    call.finish().await.expect("finish failed");
    println!("server_streaming: PASS");
}

/// ping_pong: interleaved client send / server response, 4 iterations.
async fn test_ping_pong(ch: &Channel) {
    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/FullDuplexCall", &Metadata::new(), None)
        .expect("open stream failed");

    let sizes = [(31415usize, 27182usize), (9, 8), (2653, 1828), (58979, 45904)];
    for (resp_size, req_size) in sizes {
        let req = StreamingOutputCallRequest {
            response_type: 0,
            response_parameters: vec![ResponseParameters { size: resp_size as i32, interval_us: 0 }],
            payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; req_size]) }),
        };
        call.send_message(&req).expect("send_message failed");

        let resp: StreamingOutputCallResponse =
            call.recv_message().await.expect("recv failed").expect("no response");
        let actual = resp.payload.map(|p| p.body.len()).unwrap_or(0);
        assert_eq!(actual, resp_size, "ping_pong: wrong response size");
    }
    call.close_send().expect("close_send failed");
    assert!(call.recv_message::<StreamingOutputCallResponse>().await.expect("recv failed").is_none());
    call.finish().await.expect("finish failed");
    println!("ping_pong: PASS");
}

/// empty_stream: client opens FullDuplexCall and immediately half-closes; server should return OK.
async fn test_empty_stream(ch: &Channel) {
    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/FullDuplexCall", &Metadata::new(), None)
        .expect("open stream failed");
    call.close_send().expect("close_send failed");
    assert!(call.recv_message::<StreamingOutputCallResponse>().await.expect("recv failed").is_none());
    call.finish().await.expect("finish failed");
    println!("empty_stream: PASS");
}

// ── Interop server ────────────────────────────────────────────────────────────

fn make_server() -> Server {
    let mut server = Server::new();

    server.add_service(ServiceDesc {
        name: "grpc.testing.TestService",
        methods: vec![
            // EmptyCall
            MethodDesc {
                name: "EmptyCall",
                handler: Handler::Unary(Arc::new(|_req: Bytes, _md: Metadata| {
                    Box::pin(async move {
                        let resp = Empty {};
                        let mut buf = Vec::new();
                        resp.encode(&mut buf).map_err(|e| Status::internal(format!("encode: {e}")))?;
                        Ok(Bytes::from(buf))
                    }) as BoxFuture<Result<Bytes, Status>>
                }) as UnaryHandlerFn),
            },
            // UnaryCall
            MethodDesc {
                name: "UnaryCall",
                handler: Handler::Unary(Arc::new(|req_bytes: Bytes, _md: Metadata| {
                    Box::pin(async move {
                        let req = SimpleRequest::decode(req_bytes.as_ref())
                            .map_err(|e| Status::internal(format!("decode: {e}")))?;
                        let body = vec![0u8; req.response_size as usize];
                        let resp = SimpleResponse {
                            payload: Some(Payload { r#type: req.response_type, body: Bytes::from(body) }),
                        };
                        let mut buf = Vec::new();
                        resp.encode(&mut buf).map_err(|e| Status::internal(format!("encode: {e}")))?;
                        Ok(Bytes::from(buf))
                    }) as BoxFuture<Result<Bytes, Status>>
                }) as UnaryHandlerFn),
            },
            // StreamingInputCall (client-streaming)
            MethodDesc {
                name: "StreamingInputCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        use grpc_rs::codec;
                        let mut total = 0i32;
                        while let Ok(Some(frame)) = stream.recv_message().await {
                            if let Ok(req) = StreamingInputCallRequest::decode(&frame[5..]) {
                                total += req.payload.map(|p| p.body.len() as i32).unwrap_or(0);
                            }
                        }
                        let resp = StreamingInputCallResponse { aggregated_payload_size: total };
                        let mut buf = Vec::new();
                        resp.encode(&mut buf).ok();
                        let frame = codec::encode_raw(&buf, None).unwrap();
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        stream.send_message(frame).ok();
                        stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },
            // StreamingOutputCall (server-streaming)
            MethodDesc {
                name: "StreamingOutputCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        use grpc_rs::codec;
                        // Read the single request.
                        let req = match stream.recv_message().await {
                            Ok(Some(frame)) => StreamingOutputCallRequest::decode(&frame[5..]).ok(),
                            _ => None,
                        };
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        if let Some(req) = req {
                            for params in &req.response_parameters {
                                let resp = StreamingOutputCallResponse {
                                    payload: Some(Payload {
                                        r#type: req.response_type,
                                        body: Bytes::from(vec![0u8; params.size as usize]),
                                    }),
                                };
                                let mut buf = Vec::new();
                                resp.encode(&mut buf).ok();
                                let frame = codec::encode_raw(&buf, None).unwrap();
                                if stream.send_message(frame).is_err() { break; }
                            }
                        }
                        stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },
            // FullDuplexCall (bidi-streaming)
            MethodDesc {
                name: "FullDuplexCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        use grpc_rs::codec;
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        while let Ok(Some(frame)) = stream.recv_message().await {
                            if let Ok(req) = StreamingOutputCallRequest::decode(&frame[5..]) {
                                for params in &req.response_parameters {
                                    let resp = StreamingOutputCallResponse {
                                        payload: Some(Payload {
                                            r#type: req.response_type,
                                            body: Bytes::from(vec![0u8; params.size as usize]),
                                        }),
                                    };
                                    let mut buf = Vec::new();
                                    resp.encode(&mut buf).ok();
                                    let frame = codec::encode_raw(&buf, None).unwrap();
                                    if stream.send_message(frame).is_err() { break; }
                                }
                            }
                        }
                        stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },
        ],
    });

    server
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let args = parse_args();

    if args.mode == "server" {
        let addr: SocketAddr = format!("0.0.0.0:{}", args.port).parse().unwrap();
        println!("interop server listening on {addr}");
        let server = make_server();
        server.serve(addr).await.expect("serve failed");
    } else {
        // Client mode — resolve hostname to support both IPs and DNS names
        let addr = tokio::net::lookup_host(format!("{}:{}", args.server_host, args.server_port))
            .await
            .expect("DNS lookup failed")
            .next()
            .expect("no addresses resolved");
        let channel = Channel::connect(addr).await.expect("connect failed");

        match args.test_case.as_str() {
            "empty_unary"      => test_empty_unary(&channel).await,
            "large_unary"      => test_large_unary(&channel).await,
            "client_streaming" => test_client_streaming(&channel).await,
            "server_streaming" => test_server_streaming(&channel).await,
            "ping_pong"        => test_ping_pong(&channel).await,
            "empty_stream"     => test_empty_stream(&channel).await,
            "all" => {
                test_empty_unary(&channel).await;
                test_large_unary(&channel).await;
                test_client_streaming(&channel).await;
                test_server_streaming(&channel).await;
                test_ping_pong(&channel).await;
                test_empty_stream(&channel).await;
                println!("All interop tests PASSED");
            }
            other => {
                eprintln!("Unknown test case: {other}");
                std::process::exit(1);
            }
        }
    }
}

// ── In-process self-test ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn start_interop_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let server = make_server();
        tokio::spawn(async move { server.serve(addr).await.ok(); });
        tokio::task::yield_now().await;
        addr
    }

    #[tokio::test]
    async fn interop_empty_unary() {
        let addr = start_interop_server().await;
        let ch = Channel::connect(addr).await.unwrap();
        test_empty_unary(&ch).await;
    }

    #[tokio::test]
    async fn interop_large_unary() {
        let addr = start_interop_server().await;
        let ch = Channel::connect(addr).await.unwrap();
        test_large_unary(&ch).await;
    }

    #[tokio::test]
    async fn interop_client_streaming() {
        let addr = start_interop_server().await;
        let ch = Channel::connect(addr).await.unwrap();
        test_client_streaming(&ch).await;
    }

    #[tokio::test]
    async fn interop_server_streaming() {
        let addr = start_interop_server().await;
        let ch = Channel::connect(addr).await.unwrap();
        test_server_streaming(&ch).await;
    }

    #[tokio::test]
    async fn interop_ping_pong() {
        let addr = start_interop_server().await;
        let ch = Channel::connect(addr).await.unwrap();
        test_ping_pong(&ch).await;
    }

    #[tokio::test]
    async fn interop_empty_stream() {
        let addr = start_interop_server().await;
        let ch = Channel::connect(addr).await.unwrap();
        test_empty_stream(&ch).await;
    }
}
