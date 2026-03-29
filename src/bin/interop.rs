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
//!   ping_pong, empty_stream, status_code_and_message, special_status_message,
//!   unimplemented_method, unimplemented_service,
//!   cancel_after_begin, cancel_after_first_response,
//!   timeout_on_sleeping_server, custom_metadata

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use prost::Message;

use grpc_rs::client::{Channel, StreamCall};
use grpc_rs::codec;
use grpc_rs::metadata::Metadata;
use grpc_rs::server::{BoxFuture, Handler, MethodDesc, Server, ServiceDesc, StreamingHandlerFn, UnaryHandlerFn};
use grpc_rs::status::{Code, Status};
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

/// Requested status to be returned by the server.
#[derive(Clone, PartialEq, Message)]
struct EchoStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
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
    /// If set, the server returns this status code and message.
    #[prost(message, optional, tag = "7")]
    response_status: Option<EchoStatus>,
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
    /// Delay before sending this response chunk, in microseconds.
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
        payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; 271828]) }),
        response_status: None,
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

/// status_code_and_message: server returns the status code and message requested by the client.
async fn test_status_code_and_message(ch: &Channel) {
    let req = SimpleRequest {
        response_status: Some(EchoStatus { code: Code::Unknown as i32, message: "test status message".into() }),
        ..SimpleRequest::default()
    };
    let err = ch
        .call_unary::<SimpleRequest, SimpleResponse>("/grpc.testing.TestService/UnaryCall", &req, &Metadata::new(), None)
        .await
        .expect_err("should have returned an error");
    assert_eq!(err.code, Code::Unknown, "status_code_and_message: wrong code");
    assert_eq!(err.message, "test status message", "status_code_and_message: wrong message");
    println!("status_code_and_message: PASS");
}

/// special_status_message: server returns a status with special unicode characters.
async fn test_special_status_message(ch: &Channel) {
    let msg = "\t\ntest with whitespace\r\nand Unicode BMP ☺ and non-BMP 😈\t\n";
    let req = SimpleRequest {
        response_status: Some(EchoStatus { code: Code::Unknown as i32, message: msg.into() }),
        ..SimpleRequest::default()
    };
    let err = ch
        .call_unary::<SimpleRequest, SimpleResponse>("/grpc.testing.TestService/UnaryCall", &req, &Metadata::new(), None)
        .await
        .expect_err("should have returned an error");
    assert_eq!(err.code, Code::Unknown, "special_status_message: wrong code");
    assert_eq!(err.message, msg, "special_status_message: wrong message");
    println!("special_status_message: PASS");
}

/// unimplemented_method: calling an unknown method returns UNIMPLEMENTED.
async fn test_unimplemented_method(ch: &Channel) {
    let err = ch
        .call_unary::<Empty, Empty>("/grpc.testing.TestService/UnimplementedCall", &Empty {}, &Metadata::new(), None)
        .await
        .expect_err("should have returned UNIMPLEMENTED");
    assert_eq!(err.code, Code::Unimplemented, "unimplemented_method: wrong code: {err:?}");
    println!("unimplemented_method: PASS");
}

/// unimplemented_service: calling an unknown service returns UNIMPLEMENTED.
async fn test_unimplemented_service(ch: &Channel) {
    let err = ch
        .call_unary::<Empty, Empty>("/grpc.testing.UnimplementedService/UnimplementedCall", &Empty {}, &Metadata::new(), None)
        .await
        .expect_err("should have returned UNIMPLEMENTED");
    assert_eq!(err.code, Code::Unimplemented, "unimplemented_service: wrong code: {err:?}");
    println!("unimplemented_service: PASS");
}

/// cancel_after_begin: client opens a streaming call and cancels before sending any messages.
async fn test_cancel_after_begin(ch: &Channel) {
    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/StreamingInputCall", &Metadata::new(), None)
        .expect("open stream failed");
    call.cancel();
    // After cancel, finish should return Cancelled.
    let err = call.finish().await.expect_err("should be cancelled");
    assert_eq!(err.code, Code::Cancelled, "cancel_after_begin: wrong code: {err:?}");
    println!("cancel_after_begin: PASS");
}

/// cancel_after_first_response: client cancels a bidi call after receiving the first response.
async fn test_cancel_after_first_response(ch: &Channel) {
    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/FullDuplexCall", &Metadata::new(), None)
        .expect("open stream failed");

    let req = StreamingOutputCallRequest {
        response_type: 0,
        response_parameters: vec![ResponseParameters { size: 31415, interval_us: 0 }],
        payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; 27182]) }),
    };
    call.send_message(&req).expect("send failed");
    let resp: StreamingOutputCallResponse = call.recv_message().await.expect("recv failed").expect("no response");
    assert_eq!(resp.payload.map(|p| p.body.len()).unwrap_or(0), 31415);

    call.cancel();
    let err = call.finish().await.expect_err("should be cancelled");
    assert_eq!(err.code, Code::Cancelled, "cancel_after_first_response: wrong code: {err:?}");
    println!("cancel_after_first_response: PASS");
}

/// timeout_on_sleeping_server: client sets a short deadline on a bidi call;
/// server sleeps before responding; client should get DEADLINE_EXCEEDED.
async fn test_timeout_on_sleeping_server(ch: &Channel) {
    // 1ms timeout; server will sleep 3s (interval_us = 3_000_000) before responding.
    let mut call: StreamCall = ch
        .new_streaming_call("/grpc.testing.TestService/FullDuplexCall", &Metadata::new(), Some(Duration::from_millis(1)))
        .expect("open stream failed");

    let req = StreamingOutputCallRequest {
        response_type: 0,
        response_parameters: vec![ResponseParameters { size: 8, interval_us: 3_000_000 }],
        payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; 27182]) }),
    };
    call.send_message(&req).expect("send failed");
    call.close_send().expect("close_send failed");

    // Receiving should time out before the server responds.
    let err = call.recv_message::<StreamingOutputCallResponse>().await
        .expect_err("should be deadline exceeded");
    assert_eq!(err.code, Code::DeadlineExceeded, "timeout_on_sleeping_server: wrong code: {err:?}");
    println!("timeout_on_sleeping_server: PASS");
}

/// custom_metadata: client sends custom metadata; server echoes it in initial
/// headers and trailing headers.
async fn test_custom_metadata(ch: &Channel) {
    const INITIAL_KEY:  &str = "x-grpc-test-echo-initial";
    const INITIAL_VAL:  &str = "test_initial_metadata_value";
    const TRAILING_KEY: &str = "x-grpc-test-echo-trailing-bin";
    // Binary metadata value (3 bytes): must be base64-decoded by the metadata layer.
    const TRAILING_VAL: &[u8] = b"\xab\xab\xab";

    let mut req_meta = Metadata::new();
    req_meta.insert(INITIAL_KEY, INITIAL_VAL);
    req_meta.insert_bin(TRAILING_KEY, TRAILING_VAL).expect("insert_bin failed");

    let req = SimpleRequest {
        response_size: 314159,
        payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; 271828]) }),
        ..SimpleRequest::default()
    };
    let (_resp, trailing): (SimpleResponse, Metadata) = ch
        .call_unary("/grpc.testing.TestService/UnaryCall", &req, &req_meta, None)
        .await
        .expect("custom_metadata failed");

    // Verify trailing metadata echoed back.
    let echoed = trailing.get_bin(TRAILING_KEY)
        .expect("missing trailing bin metadata");
    assert_eq!(echoed, TRAILING_VAL, "custom_metadata: trailing bin metadata mismatch");

    println!("custom_metadata: PASS");
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
                        let mut buf = Vec::new();
                        Empty {}.encode(&mut buf).map_err(|e| Status::internal(format!("encode: {e}")))?;
                        Ok(Bytes::from(buf))
                    }) as BoxFuture<Result<Bytes, Status>>
                }) as UnaryHandlerFn),
            },

            // UnaryCall — implemented as a streaming handler so we can set custom
            // initial headers and trailing headers (needed for custom_metadata test).
            MethodDesc {
                name: "UnaryCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, md: Metadata| {
                    Box::pin(async move {
                        // Read the single request message.
                        let frame = match stream.recv_message().await {
                            Ok(Some(f)) => f,
                            _ => {
                                stream.send_trailers(&Status::internal("no request"), &http::HeaderMap::new()).ok();
                                return;
                            }
                        };

                        let req = match SimpleRequest::decode(&frame[5..]) {
                            Ok(r) => r,
                            Err(e) => {
                                stream.send_trailers(&Status::internal(format!("decode: {e}")), &http::HeaderMap::new()).ok();
                                return;
                            }
                        };

                        // Build initial response headers, echoing x-grpc-test-echo-initial if present.
                        let mut init_headers = http::HeaderMap::new();
                        if let Some(v) = md.get("x-grpc-test-echo-initial") {
                            if let Ok(hv) = http::HeaderValue::from_str(v) {
                                init_headers.insert(
                                    http::header::HeaderName::from_static("x-grpc-test-echo-initial"),
                                    hv,
                                );
                            }
                        }
                        stream.send_headers(&init_headers).ok();

                        // If response_status is set, return that status instead of a response.
                        if let Some(echo_status) = req.response_status {
                            if echo_status.code != 0 {
                                let code = grpc_rs::status::Code::try_from(echo_status.code as u32)
                                    .unwrap_or(grpc_rs::status::Code::Unknown);
                                let status = Status::new(code, echo_status.message);
                                stream.send_trailers(&status, &http::HeaderMap::new()).ok();
                                return;
                            }
                        }

                        // Normal response.
                        let resp = SimpleResponse {
                            payload: Some(Payload {
                                r#type: req.response_type,
                                body: Bytes::from(vec![0u8; req.response_size as usize]),
                            }),
                        };
                        let mut buf = Vec::new();
                        if resp.encode(&mut buf).is_ok() {
                            let f = codec::encode_raw(&buf, None).unwrap();
                            stream.send_message(f).ok();
                        }

                        // Build trailing headers, echoing x-grpc-test-echo-trailing-bin if present.
                        let mut trail_headers = http::HeaderMap::new();
                        if let Some(bin_val) = md.get_bin("x-grpc-test-echo-trailing-bin") {
                            use base64::Engine as _;
                            let encoded = base64::engine::general_purpose::STANDARD.encode(&bin_val);
                            if let Ok(hv) = http::HeaderValue::from_str(&encoded) {
                                trail_headers.insert(
                                    http::header::HeaderName::from_static("x-grpc-test-echo-trailing-bin"),
                                    hv,
                                );
                            }
                        }
                        stream.send_trailers(&Status::ok(), &trail_headers).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },

            // StreamingInputCall (client-streaming)
            MethodDesc {
                name: "StreamingInputCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
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

            // StreamingOutputCall (server-streaming) — supports interval_us delay.
            MethodDesc {
                name: "StreamingOutputCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        let req = match stream.recv_message().await {
                            Ok(Some(frame)) => StreamingOutputCallRequest::decode(&frame[5..]).ok(),
                            _ => None,
                        };
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        if let Some(req) = req {
                            for params in &req.response_parameters {
                                if params.interval_us > 0 {
                                    tokio::time::sleep(Duration::from_micros(params.interval_us as u64)).await;
                                }
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

            // FullDuplexCall (bidi-streaming) — supports interval_us delay.
            MethodDesc {
                name: "FullDuplexCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        while let Ok(Some(frame)) = stream.recv_message().await {
                            if let Ok(req) = StreamingOutputCallRequest::decode(&frame[5..]) {
                                for params in &req.response_parameters {
                                    if params.interval_us > 0 {
                                        tokio::time::sleep(Duration::from_micros(params.interval_us as u64)).await;
                                    }
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
            "empty_unary"                 => test_empty_unary(&channel).await,
            "large_unary"                 => test_large_unary(&channel).await,
            "client_streaming"            => test_client_streaming(&channel).await,
            "server_streaming"            => test_server_streaming(&channel).await,
            "ping_pong"                   => test_ping_pong(&channel).await,
            "empty_stream"                => test_empty_stream(&channel).await,
            "status_code_and_message"     => test_status_code_and_message(&channel).await,
            "special_status_message"      => test_special_status_message(&channel).await,
            "unimplemented_method"        => test_unimplemented_method(&channel).await,
            "unimplemented_service"       => test_unimplemented_service(&channel).await,
            "cancel_after_begin"          => test_cancel_after_begin(&channel).await,
            "cancel_after_first_response" => test_cancel_after_first_response(&channel).await,
            "timeout_on_sleeping_server"  => test_timeout_on_sleeping_server(&channel).await,
            "custom_metadata"             => test_custom_metadata(&channel).await,
            "all" => {
                test_empty_unary(&channel).await;
                test_large_unary(&channel).await;
                test_client_streaming(&channel).await;
                test_server_streaming(&channel).await;
                test_ping_pong(&channel).await;
                test_empty_stream(&channel).await;
                test_status_code_and_message(&channel).await;
                test_special_status_message(&channel).await;
                test_unimplemented_method(&channel).await;
                test_unimplemented_service(&channel).await;
                test_cancel_after_begin(&channel).await;
                test_cancel_after_first_response(&channel).await;
                test_timeout_on_sleeping_server(&channel).await;
                test_custom_metadata(&channel).await;
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
        test_empty_unary(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_large_unary() {
        let addr = start_interop_server().await;
        test_large_unary(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_client_streaming() {
        let addr = start_interop_server().await;
        test_client_streaming(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_server_streaming() {
        let addr = start_interop_server().await;
        test_server_streaming(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_ping_pong() {
        let addr = start_interop_server().await;
        test_ping_pong(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_empty_stream() {
        let addr = start_interop_server().await;
        test_empty_stream(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_status_code_and_message() {
        let addr = start_interop_server().await;
        test_status_code_and_message(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_special_status_message() {
        let addr = start_interop_server().await;
        test_special_status_message(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_unimplemented_method() {
        let addr = start_interop_server().await;
        test_unimplemented_method(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_unimplemented_service() {
        let addr = start_interop_server().await;
        test_unimplemented_service(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_cancel_after_begin() {
        let addr = start_interop_server().await;
        test_cancel_after_begin(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_cancel_after_first_response() {
        let addr = start_interop_server().await;
        test_cancel_after_first_response(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_timeout_on_sleeping_server() {
        let addr = start_interop_server().await;
        test_timeout_on_sleeping_server(&Channel::connect(addr).await.unwrap()).await;
    }

    #[tokio::test]
    async fn interop_custom_metadata() {
        let addr = start_interop_server().await;
        test_custom_metadata(&Channel::connect(addr).await.unwrap()).await;
    }
}
