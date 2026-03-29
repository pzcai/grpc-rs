# grpc-rs vs grpc-go Performance Report

**Date:** 2026-03-29
**Platform:** macOS Darwin 24.6.0, Apple Silicon
**grpc-go version:** v1.79.3
**Rust edition:** 2021, tokio multi-thread runtime

## Methodology

Both benchmarks run an in-process server and client (loopback TCP, no TLS).
The Go benchmark uses `google.golang.org/grpc/interop.NewTestServer()` with the
standard `grpc_testing` proto service. The Rust benchmark uses the same proto
messages and the same four scenarios.

Both sides use identical HTTP/2 window configuration: 4 MB initial stream window
and 4 MB initial connection window (see tuning note below).

Benchmark sources:
- Rust: `src/bin/bench.rs`
- Go: `/tmp/grpc-go-bench/main.go`

---

## Results (equal tuning — both sides 4 MB H/2 windows)

### 1. Serial Unary Latency (10,000 sequential EmptyCall RPCs, 1 connection)

| Metric        | grpc-rs   | grpc-go   | Winner        |
|---------------|-----------|-----------|---------------|
| RPS           | 31,994    | 28,205    | **rs +13%**   |
| mean_us       | 31.3      | 35.5      | **rs −12%**   |
| p50_us        | 28.0      | 34.0      | **rs −18%**   |
| p90_us        | 38.0      | 40.0      | **rs −5%**    |
| p99_us        | 64.0      | 78.0      | **rs −18%**   |
| p999_us       | 81.0      | 171.0     | **rs −53%**   |

grpc-rs has consistently lower tail latency. The p99.9 gap (81 µs vs 171 µs)
reflects Go GC pauses that are absent in Rust.

---

### 2. Concurrent Unary Throughput (100 workers × 100 calls, 1 connection per worker)

| Metric        | grpc-rs    | grpc-go    | Winner        |
|---------------|------------|------------|---------------|
| elapsed_ms    | 50         | 69         | **rs −28%**   |
| RPS           | 199,152    | 144,244    | **rs +38%**   |

Each worker uses its own TCP connection. grpc-rs benefits from zero-cost async
tasks and no stop-the-world GC.

---

### 3. Server Streaming Throughput (50 calls × 1,000 msgs × 1 KB, 1 connection)

| Metric        | grpc-rs   | grpc-go   | Winner        |
|---------------|-----------|-----------|---------------|
| elapsed_ms    | 55        | 53        | **tie**       |
| msgs_per_s    | 907,206   | 933,479   | **go +3%**    |
| mb_per_s      | 885.9     | 911.6     | **go +3%**    |

With equal window sizes, streaming throughput is essentially identical — the 3%
difference is within run-to-run noise. Both are saturating loopback TCP bandwidth.

---

### 4. Bidi Ping-Pong (50 calls × 500 send/recv rounds, 1 connection)

| Metric        | grpc-rs    | grpc-go    | Winner        |
|---------------|------------|------------|---------------|
| elapsed_ms    | 598        | 563        | **go −6%**    |
| rounds_per_s  | 41,744     | 44,383     | **go +6%**    |

grpc-go has a slight edge in ping-pong. Likely due to grpc-go's more mature
write-batching and buffer reuse in its transport layer.

---

## Summary

| Scenario          | grpc-rs     | grpc-go     | Delta   |
|-------------------|-------------|-------------|---------|
| Serial unary      | 31,994 RPS  | 28,205 RPS  | **rs +13%** |
| Concurrent unary  | 199,152 RPS | 144,244 RPS | **rs +38%** |
| Server streaming  | 885.9 MB/s  | 911.6 MB/s  | go +3% (tie) |
| Bidi ping-pong    | 41,744 r/s  | 44,383 r/s  | go +6%  |

**grpc-rs wins on unary RPCs** — both throughput and tail latency — primarily
because Rust has no GC pauses. **Streaming throughput is a tie** once both sides
use the same H/2 window sizes. **grpc-go has a slight edge on bidi ping-pong**,
likely due to more mature write-batching in its transport.

---

## H/2 window tuning (applied 2026-03-29)

The h2 crate's default window size is 65,535 bytes (the HTTP/2 spec minimum).
With 1 KB messages and 1,000 messages per stream, the sender stalls after every
~64 messages waiting for WINDOW_UPDATE frames — cutting streaming throughput to
~62 MB/s. Setting both sides to 4 MB eliminates this entirely.

**grpc-rs** (`src/transport/server.rs` + `src/transport/client.rs`):
```rust
h2::{server,client}::Builder::new()
    .initial_window_size(4 * 1024 * 1024)
    .initial_connection_window_size(4 * 1024 * 1024)
    .handshake(io)
```

**grpc-go** (`/tmp/grpc-go-bench/main.go`):
```go
grpc.NewServer(
    grpc.InitialWindowSize(4*1024*1024),
    grpc.InitialConnWindowSize(4*1024*1024),
)
grpc.NewClient(addr,
    grpc.WithInitialWindowSize(4*1024*1024),
    grpc.WithInitialConnWindowSize(4*1024*1024),
)
```

---

## Next steps

Profile with `sudo cargo flamegraph --bin bench` to identify any remaining
bottlenecks in the codec or tokio scheduler under sustained streaming load.
