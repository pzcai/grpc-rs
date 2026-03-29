//! Performance benchmark for grpc-rs.
//!
//! Runs 4 scenarios against an in-process grpc-rs server:
//!   1. serial_unary      – sequential EmptyCall latency (10 k iterations)
//!   2. concurrent_unary  – 100 workers × 100 calls each, measures RPS
//!   3. server_streaming  – 50 calls × 1 000 × 1 KB responses, measures MB/s
//!   4. bidi_pingpong     – 50 calls × 500 rounds, measures rounds/s
//!
//! Build and run:
//!   cargo build --release --bin bench
//!   ./target/release/bench

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::Barrier;

use grpc_rs::client::{Channel, StreamCall};
use grpc_rs::codec;
use grpc_rs::metadata::Metadata;
use grpc_rs::server::{BoxFuture, Handler, MethodDesc, Server, ServiceDesc, StreamingHandlerFn, UnaryHandlerFn};
use grpc_rs::status::Status;
use grpc_rs::transport::ServerStream;

// ── Protobuf message definitions (mirrors grpc/testing/test.proto) ───────────

#[derive(Clone, PartialEq, Message)] struct Empty {}

#[derive(Clone, PartialEq, Message)]
struct Payload {
    #[prost(int32, tag = "1")] r#type: i32,
    #[prost(bytes = "bytes", tag = "2")] body: Bytes,
}

#[derive(Clone, PartialEq, Message)]
struct EchoStatus {
    #[prost(int32, tag = "1")] code: i32,
    #[prost(string, tag = "2")] message: String,
}

#[derive(Clone, PartialEq, Message)]
struct SimpleRequest {
    #[prost(int32, tag = "1")] response_type: i32,
    #[prost(int32, tag = "2")] response_size: i32,
    #[prost(message, optional, tag = "3")] payload: Option<Payload>,
    #[prost(message, optional, tag = "7")] response_status: Option<EchoStatus>,
}

#[derive(Clone, PartialEq, Message)]
struct SimpleResponse {
    #[prost(message, optional, tag = "1")] payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingInputCallRequest {
    #[prost(message, optional, tag = "1")] payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingInputCallResponse {
    #[prost(int32, tag = "1")] aggregated_payload_size: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ResponseParameters {
    #[prost(int32, tag = "1")] size: i32,
    #[prost(int32, tag = "2")] interval_us: i32,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingOutputCallRequest {
    #[prost(int32, tag = "1")] response_type: i32,
    #[prost(message, repeated, tag = "2")] response_parameters: Vec<ResponseParameters>,
    #[prost(message, optional, tag = "3")] payload: Option<Payload>,
}

#[derive(Clone, PartialEq, Message)]
struct StreamingOutputCallResponse {
    #[prost(message, optional, tag = "1")] payload: Option<Payload>,
}

// ── In-process server ─────────────────────────────────────────────────────────

fn make_server() -> Server {
    let mut server = Server::new();
    server.add_service(ServiceDesc {
        name: "grpc.testing.TestService",
        methods: vec![
            MethodDesc {
                name: "EmptyCall",
                handler: Handler::Unary(Arc::new(|_req: Bytes, _md: Metadata| {
                    Box::pin(async move {
                        let mut buf = Vec::new();
                        Empty {}.encode(&mut buf).map_err(|e| Status::internal(format!("{e}")))?;
                        Ok(Bytes::from(buf))
                    }) as BoxFuture<Result<Bytes, Status>>
                }) as UnaryHandlerFn),
            },
            MethodDesc {
                name: "UnaryCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        let frame = match stream.recv_message().await {
                            Ok(Some(f)) => f,
                            _ => { stream.send_trailers(&Status::internal("no req"), &http::HeaderMap::new()).ok(); return; }
                        };
                        let req = match SimpleRequest::decode(&frame[5..]) {
                            Ok(r) => r,
                            Err(e) => { stream.send_trailers(&Status::internal(format!("{e}")), &http::HeaderMap::new()).ok(); return; }
                        };
                        if let Some(es) = req.response_status {
                            if es.code != 0 {
                                let code = grpc_rs::status::Code::try_from(es.code as u32).unwrap_or(grpc_rs::status::Code::Unknown);
                                stream.send_headers(&http::HeaderMap::new()).ok();
                                stream.send_trailers(&Status::new(code, es.message), &http::HeaderMap::new()).ok();
                                return;
                            }
                        }
                        let resp = SimpleResponse {
                            payload: Some(Payload { r#type: req.response_type, body: Bytes::from(vec![0u8; req.response_size as usize]) }),
                        };
                        let mut buf = Vec::new();
                        resp.encode(&mut buf).ok();
                        let f = codec::encode_raw(&buf, None).unwrap();
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        stream.send_message(f).ok();
                        stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },
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
                        let mut buf = Vec::new(); resp.encode(&mut buf).ok();
                        let frame = codec::encode_raw(&buf, None).unwrap();
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        stream.send_message(frame).ok();
                        stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },
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
                                let resp = StreamingOutputCallResponse {
                                    payload: Some(Payload { r#type: req.response_type, body: Bytes::from(vec![0u8; params.size as usize]) }),
                                };
                                let mut buf = Vec::new(); resp.encode(&mut buf).ok();
                                let frame = codec::encode_raw(&buf, None).unwrap();
                                if stream.send_message(frame).is_err() { break; }
                            }
                        }
                        stream.send_trailers(&Status::ok(), &http::HeaderMap::new()).ok();
                    }) as BoxFuture<()>
                }) as StreamingHandlerFn),
            },
            MethodDesc {
                name: "FullDuplexCall",
                handler: Handler::Streaming(Arc::new(|mut stream: ServerStream, _md: Metadata| {
                    Box::pin(async move {
                        stream.send_headers(&http::HeaderMap::new()).ok();
                        while let Ok(Some(frame)) = stream.recv_message().await {
                            if let Ok(req) = StreamingOutputCallRequest::decode(&frame[5..]) {
                                for params in &req.response_parameters {
                                    let resp = StreamingOutputCallResponse {
                                        payload: Some(Payload { r#type: req.response_type, body: Bytes::from(vec![0u8; params.size as usize]) }),
                                    };
                                    let mut buf = Vec::new(); resp.encode(&mut buf).ok();
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

async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let server = make_server();
    tokio::spawn(async move { server.serve(addr).await.ok(); });
    tokio::task::yield_now().await;
    addr
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((p / 100.0) * sorted.len() as f64) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Benchmark 1: Serial unary latency ────────────────────────────────────────

async fn bench_serial_unary(ch: &Channel) {
    const N: usize = 10_000;
    let mut latencies_us: Vec<f64> = Vec::with_capacity(N);

    for _ in 0..N {
        let t0 = Instant::now();
        let _: (Empty, _) = ch
            .call_unary("/grpc.testing.TestService/EmptyCall", &Empty {}, &Metadata::new(), None)
            .await
            .expect("EmptyCall failed");
        latencies_us.push(t0.elapsed().as_micros() as f64);
    }

    latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = latencies_us.iter().sum::<f64>() / N as f64;
    let total_ms = latencies_us.iter().sum::<f64>() / 1_000.0;

    println!("[serial_unary]");
    println!("  iterations : {N}");
    println!("  total_ms   : {:.1}", total_ms);
    println!("  rps        : {:.0}", N as f64 / (total_ms / 1_000.0));
    println!("  mean_us    : {:.1}", mean);
    println!("  p50_us     : {:.1}", percentile(&latencies_us, 50.0));
    println!("  p90_us     : {:.1}", percentile(&latencies_us, 90.0));
    println!("  p99_us     : {:.1}", percentile(&latencies_us, 99.0));
    println!("  p999_us    : {:.1}", percentile(&latencies_us, 99.9));
}

// ── Benchmark 2: Concurrent unary throughput ─────────────────────────────────

async fn bench_concurrent_unary(addr: SocketAddr) {
    const WORKERS: usize = 100;
    const CALLS_PER_WORKER: usize = 100;
    const TOTAL: usize = WORKERS * CALLS_PER_WORKER;

    // Each worker gets its own Channel (its own H2 connection) to avoid
    // the single-connection Mutex becoming the bottleneck.
    let mut channels = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        channels.push(Arc::new(Channel::connect(addr).await.expect("connect")));
    }

    let barrier = Arc::new(Barrier::new(WORKERS));
    let t_start = Instant::now();

    let handles: Vec<_> = channels
        .into_iter()
        .map(|ch| {
            let b = Arc::clone(&barrier);
            tokio::spawn(async move {
                b.wait().await;
                for _ in 0..CALLS_PER_WORKER {
                    let _: (Empty, _) = ch
                        .call_unary("/grpc.testing.TestService/EmptyCall", &Empty {}, &Metadata::new(), None)
                        .await
                        .expect("EmptyCall failed");
                }
            })
        })
        .collect();

    for h in handles { h.await.unwrap(); }
    let elapsed = t_start.elapsed();
    let rps = TOTAL as f64 / elapsed.as_secs_f64();

    println!("[concurrent_unary]");
    println!("  workers    : {WORKERS}");
    println!("  total_calls: {TOTAL}");
    println!("  elapsed_ms : {:.1}", elapsed.as_millis());
    println!("  rps        : {:.0}", rps);
}

// ── Benchmark 3: Server-streaming throughput ─────────────────────────────────

async fn bench_server_streaming(ch: &Channel) {
    const CALLS: usize = 50;
    const MSGS_PER_CALL: usize = 1_000;
    const MSG_SIZE: usize = 1_024; // 1 KB
    const TOTAL_MSGS: usize = CALLS * MSGS_PER_CALL;
    const TOTAL_BYTES: usize = TOTAL_MSGS * MSG_SIZE;

    let req = StreamingOutputCallRequest {
        response_type: 0,
        response_parameters: (0..MSGS_PER_CALL)
            .map(|_| ResponseParameters { size: MSG_SIZE as i32, interval_us: 0 })
            .collect(),
        payload: None,
    };

    let t0 = Instant::now();

    for _ in 0..CALLS {
        let mut call: StreamCall = ch
            .new_streaming_call("/grpc.testing.TestService/StreamingOutputCall", &Metadata::new(), None)
            .expect("open stream");
        call.send_message(&req).expect("send");
        call.close_send().expect("close_send");
        let mut count = 0usize;
        while call.recv_message::<StreamingOutputCallResponse>().await.expect("recv").is_some() {
            count += 1;
        }
        assert_eq!(count, MSGS_PER_CALL, "unexpected message count");
        call.finish().await.expect("finish");
    }

    let elapsed = t0.elapsed();
    let msgs_per_s = TOTAL_MSGS as f64 / elapsed.as_secs_f64();
    let mb_per_s = TOTAL_BYTES as f64 / elapsed.as_secs_f64() / 1_048_576.0;

    println!("[server_streaming]");
    println!("  calls      : {CALLS}");
    println!("  msgs/call  : {MSGS_PER_CALL}");
    println!("  msg_kb     : {}", MSG_SIZE / 1024);
    println!("  elapsed_ms : {:.1}", elapsed.as_millis());
    println!("  msgs_per_s : {:.0}", msgs_per_s);
    println!("  mb_per_s   : {:.1}", mb_per_s);
}

// ── Benchmark 4: Bidi ping-pong throughput ───────────────────────────────────

async fn bench_bidi_pingpong(ch: &Channel) {
    const CALLS: usize = 50;
    const ROUNDS_PER_CALL: usize = 500;
    const TOTAL_ROUNDS: usize = CALLS * ROUNDS_PER_CALL;

    let req = StreamingOutputCallRequest {
        response_type: 0,
        response_parameters: vec![ResponseParameters { size: 1, interval_us: 0 }],
        payload: Some(Payload { r#type: 0, body: Bytes::from(vec![0u8; 1]) }),
    };

    let t0 = Instant::now();

    for _ in 0..CALLS {
        let mut call: StreamCall = ch
            .new_streaming_call("/grpc.testing.TestService/FullDuplexCall", &Metadata::new(), None)
            .expect("open stream");

        for _ in 0..ROUNDS_PER_CALL {
            call.send_message(&req).expect("send");
            let _: Option<StreamingOutputCallResponse> = call.recv_message().await.expect("recv");
        }
        call.close_send().expect("close_send");
        while call.recv_message::<StreamingOutputCallResponse>().await.expect("drain").is_some() {}
        call.finish().await.expect("finish");
    }

    let elapsed = t0.elapsed();
    let rounds_per_s = TOTAL_ROUNDS as f64 / elapsed.as_secs_f64();

    println!("[bidi_pingpong]");
    println!("  calls       : {CALLS}");
    println!("  rounds/call : {ROUNDS_PER_CALL}");
    println!("  total_rounds: {TOTAL_ROUNDS}");
    println!("  elapsed_ms  : {:.1}", elapsed.as_millis());
    println!("  rounds_per_s: {:.0}", rounds_per_s);
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("=== grpc-rs benchmark ===");
    println!();

    let addr = start_server().await;
    println!("server: {addr}");
    println!();

    // Warm-up: 100 calls to prime the H2 connection and JIT paths.
    let ch = Channel::connect(addr).await.expect("connect");
    for _ in 0..100 {
        let _: (Empty, _) = ch.call_unary("/grpc.testing.TestService/EmptyCall", &Empty {}, &Metadata::new(), None).await.expect("warmup");
    }

    bench_serial_unary(&ch).await;
    println!();
    bench_concurrent_unary(addr).await;
    println!();
    bench_server_streaming(&ch).await;
    println!();
    bench_bidi_pingpong(&ch).await;
    println!();

    println!("=== done ===");
}
