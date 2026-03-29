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

Benchmark sources:
- Rust: `src/bin/bench.rs`
- Go: `/tmp/grpc-go-bench/main.go`

---

## Results

### 1. Serial Unary Latency (10,000 sequential EmptyCall RPCs, 1 connection)

| Metric        | grpc-rs   | grpc-go   | Winner    |
|---------------|-----------|-----------|-----------|
| RPS           | 32,900    | 29,000    | **rs +13%**   |
| mean_us       | 30.4      | 34.5      | **rs −12%**   |
| p50_us        | 28.0      | 31.0      | **rs −10%**   |
| p90_us        | 38.0      | 47.0      | **rs −19%**   |
| p99_us        | 52.0      | 78.0      | **rs −33%**   |
| p999_us       | 68.0      | 193.0     | **rs −65%**   |

grpc-rs shows consistently lower tail latency. The p99.9 gap (68 µs vs 193 µs) reflects
the Go GC pauses absent from Rust.

---

### 2. Concurrent Unary Throughput (100 workers × 100 calls, 1 connection per worker)

| Metric        | grpc-rs    | grpc-go    | Winner     |
|---------------|------------|------------|------------|
| elapsed_ms    | 51         | 69         | **rs −26%**    |
| RPS           | 196,000    | 144,000    | **rs +36%**    |

Each worker uses its own TCP connection. grpc-rs achieves significantly higher
throughput under concurrent load, benefiting from zero-cost async tasks and
no stop-the-world GC.

---

### 3. Server Streaming Throughput (50 calls × 1,000 msgs × 1 KB, 1 connection)

| Metric        | grpc-rs   | grpc-go   | Winner    |
|---------------|-----------|-----------|-----------|
| elapsed_ms    | 780       | 593       | **go −24%**   |
| msgs_per_s    | 64,100    | 84,200    | **go +31%**   |
| mb_per_s      | 62.6      | 82.2      | **go +31%**   |

grpc-go outperforms grpc-rs on streaming throughput. The gap is likely due to
grpc-go's more mature HTTP/2 flow-control tuning and buffer sizing in its
transport layer. The h2 crate's default window sizes are more conservative.

---

### 4. Bidi Ping-Pong (50 calls × 500 send/recv rounds, 1 connection)

| Metric        | grpc-rs    | grpc-go    | Winner     |
|---------------|------------|------------|------------|
| elapsed_ms    | 597        | 648        | **rs −8%**     |
| rounds_per_s  | 41,900     | 38,600     | **rs +9%**     |

grpc-rs has a slight edge in the ping-pong pattern where per-message overhead
and scheduling latency dominate over raw bandwidth.

---

## Summary

| Scenario              | grpc-rs RPS | grpc-go RPS | Delta    |
|-----------------------|-------------|-------------|----------|
| Serial unary          | 32,900      | 29,000      | +13%     |
| Concurrent unary      | 196,000     | 144,000     | +36%     |
| Server streaming      | 64,100 m/s  | 84,200 m/s  | −24%     |
| Bidi ping-pong        | 41,900 r/s  | 38,600 r/s  | +9%      |

**grpc-rs leads in:** low-latency unary RPCs, high-concurrency throughput, ping-pong
round-trip speed, and tail-latency consistency (p99.9 is 65% lower for serial unary).

**grpc-go leads in:** raw streaming throughput. This is expected given grpc-go's
production-hardened HTTP/2 flow-control implementation vs. the h2 crate's default
settings.

## Next steps (streaming gap)

To close the streaming throughput gap, investigate:
1. Increase HTTP/2 initial window sizes (`h2::server::Builder::initial_window_size`,
   `h2::client::Builder::initial_window_size`) — default is 65,535 bytes; try 4 MB.
2. Increase `initial_connection_window_size` to match.
3. Profile with `cargo flamegraph` to identify the bottleneck (codec, h2 flow control,
   or tokio scheduler).
