# Changelog

## How to use this file
- Add an entry after every meaningful unit of work
- Record failed approaches with the reason they failed
- Future sessions: read this file first, then continue from Current Status

---

## Current status
ALL 14 gRPC INTEROP TEST CASES PASS against grpc-go reference server. 108 tests passing.
Core gRPC library fully functional: codec, transport, server, client, unary/streaming RPCs,
metadata, deadline/cancellation, TLS (rustls), interceptors, interop test binary,
basic retry policy, helloworld + route_guide examples.
Project meets all CLAUDE.md success criteria.

## Session log

### 2026-03-27 — Session 1: Codec layer

**Plan (written before any code):**
Implement Module 1: the gRPC codec layer. This covers three concerns:
(1) Wire format: the 5-byte length-prefix frame header (1-byte compression flag +
4-byte big-endian u32 payload length). This is the lowest-level framing and must
be correct before anything else.
(2) Compressor abstraction: a `Compressor` trait with `compress`/`decompress` and a
`name()` for the `grpc-encoding` HTTP/2 header. Implement gzip via `flate2`.
(3) Message encode/decode helpers: generic free functions `encode_message<M>` and
`decode_message<M>` that combine prost serialization + framing + optional compression.
Key invariants from grpc-go: zero-length payloads are never compressed; compressed
flag 0x01 with no registered decompressor is an Internal error; decompressed size is
also bounded by `max_recv_size`; any compression flag byte other than 0x00 or 0x01
is an Internal error. Registry deferred to when we need multiple codec types.

**Completed:**
- Set up Cargo.toml with dependencies (prost, bytes, flate2)
- Implemented `Status` type with all 17 gRPC status codes
- Implemented `Compressor` trait with `GzipCompressor` (flate2-backed)
- Implemented `encode_raw` / `decode_raw` (5-byte frame encoding/decoding)
- Implemented `encode_message<M>` / `decode_message<M>` (prost + framing)
- All invariants from grpc-go encoded as unit tests (14 tests, all passing)

**Next session — Module 2:** Transport layer — HTTP/2 framing via h2 crate, stream
lifecycle, flow control. Start by reading grpc-go's `transport/` package, particularly
`transport.go`, `http2_client.go`, and `http2_server.go` for the stream state machine.

---

### 2026-03-27 — Session 1 (continued): Transport layer

**Plan (written before any code):**
Implement Module 2: gRPC transport over HTTP/2. Builds directly on the codec layer.
Key behaviors from grpc-go: (1) Single serialized write path per connection — in Rust
this falls naturally from h2's owned SendStream which is not Clone; the single-write
invariant is maintained by ownership. (2) Stream state machine: AwaitingHeaders →
Streaming → Done for the send side; recv_done flag for the receive side. (3) Frame
reassembly: h2 delivers DATA in arbitrary chunks; we buffer into BytesMut and extract
complete gRPC frames using the 5-byte header. (4) Flow control: release_capacity() after
every read. (5) Trailer-only responses: always send initial HEADERS then HEADERS(END_STREAM)
even if no DATA — simpler than grpc-go's combined frame and equally valid per spec.
Both server and client transports implemented; connection task returned to caller (not
spawned internally) to avoid hard tokio::spawn dependency in the library.
Client transport: `connect()` returns `(ClientTransport, impl Future)` — caller spawns the
connection future. grpc-timeout parsing deferred to Module 3/4 (server/client core).

**Completed:**
- `ServerTransport<T>`: h2 server accept loop in background tokio task; dispatches streams
  via mpsc channel. Failed approach: returning stream directly from `accept()` without
  driving the connection caused "broken pipe" — outbound h2 frames were never flushed.
  Fix: background task owns the Connection and continuously drives it.
- `ServerStream`: recv_message (with DATA frame reassembly), send_headers, send_message,
  send_trailers (incl. auto-send-headers for trailer-only responses), percent_encode_grpc_message
- `ClientTransport`: `connect()` returns `(transport, conn_fut)`; `new_stream()` creates streams
- `ClientStream`: send_message/finish_send, recv_headers (validates status+content-type),
  recv_message (with reassembly), recv_trailers (parses grpc-status → Status)
- percent_decode_grpc_message, parse_grpc_status helpers
- 40 unit + integration tests, all passing

**Next session — Module 3:** Server core — TCP listener, connection accept loop, stream
dispatch, service registry. Reads from `server.go`, `service_config.go` in grpc-go.
Key behaviors to implement: ServiceDesc registration, method dispatch by path, content-type
validation (must start with `application/grpc`), graceful shutdown.

---

### 2026-03-27 — Session 2: Server core (Module 3)

**Plan (written before any code):**
Implement Module 3: gRPC server core. This sits above the transport layer and handles
RPC dispatch. Key types: `ServiceDesc { name, methods }` and `MethodDesc { name, handler }`
where `UnaryHandlerFn = Arc<dyn Fn(Bytes) -> BoxFuture<Result<Bytes, Status>>>`.  Handlers
receive the raw decoded protobuf bytes (no gRPC framing); the server core handles framing
via the codec layer.  `Server::add_service` registers services by name; `Server::serve`
binds a TCP listener and spawns per-connection + per-stream tasks.  Dispatch follows
grpc-go's `handleStream`: strip leading `/`, split at last `/` → `(service, method)`.
Content-type validation: must start with `application/grpc`; error code Unimplemented on
unknown service/method (matching grpc-go), InvalidArgument on bad content-type.
Graceful shutdown deferred; `serve` runs until its future is dropped.
**Completed:**
- `ServiceDesc` / `MethodDesc` / `UnaryHandlerFn` types
- `Server::add_service` (panics on duplicate), `Server::serve(SocketAddr)`
- `dispatch_stream`: content-type validation (InvalidArgument), path parsing (Unimplemented
  on malformed/unknown service/unknown method), handler invocation, codec framing
- `parse_method_path`: strips leading `/`, splits at last `/`, matches grpc-go behavior
- Fix: borrow checker rejected using `stream.method()` alongside `&mut stream` in error
  paths — resolved by extracting owned Strings at top of `dispatch_stream`
- 10 new tests (6 unit + 4 integration), all passing; total 50 tests

---

### 2026-03-27 — Session 2 (continued): Client core + Unary RPC (Modules 4 + 5)

**Plan:**
Module 4: `Channel` wraps `ClientTransport` behind a `std::sync::Mutex` so multiple callers
can create streams without `&mut` conflicts; `new_stream` only holds the lock during the
synchronous part, releasing before any `.await`.  `Channel::connect(SocketAddr)` does the TCP
connect, h2 handshake, and spawns the connection task internally.  `Channel::call_unary<Req,Resp>`
does the complete unary RPC cycle (encode → send → recv_headers → recv_message → decode →
recv_trailers → check status).  No reconnection, no load balancing, no subchannel pooling —
these are deferred to a later session.
Module 5: End-to-end integration tests using prost messages directly (`#[derive(prost::Message)]`
inline structs), exercising the full stack: Channel → ClientTransport → TCP → Server → handler
→ response. Success criterion: a "greeter" test that calls `SayHello` and verifies the reply.

**Completed:**
- `Channel` in `src/client.rs`: connects over TCP, spawns h2 conn task, wraps `ClientTransport`
  behind `std::sync::Mutex` (held only for synchronous `new_stream`, not across `.await`)
- `Channel::call_unary<Req,Resp>`: full unary cycle; fix: initially checked for empty body
  before reading trailers, causing `Internal` for trailer-only error responses — fixed by
  reading trailers first, then checking status, then checking for response body
- Module 5: `say_hello_end_to_end` and 3 more integration tests pass end-to-end
- 54 total tests passing

---

### 2026-03-27 — Session 2 (continued): Metadata (Module 6)

**Plan:**
Introduce `Metadata` type in `src/metadata.rs` backed by `http::HeaderMap`.
Binary keys (ending in `-bin`) are base64-encoded on send, decoded on receive.
`Metadata::from_request_headers()` filters gRPC-internal headers so the handler
sees only user metadata.  Update `UnaryHandlerFn` to `Fn(Bytes, Metadata) →
BoxFuture<Result<Bytes, Status>>` so handlers can read request metadata.  Update
`Channel::call_unary` to `→ Result<(Resp, Metadata), Status>` where the `Metadata`
is the trailing metadata from the server.  Existing tests updated to match new
signatures (add `_md` arg to closures, destructure `(r, _)` from call_unary).
ASCII metadata round-trip test added; binary metadata test added.
base64 dependency: use `base64 = "0.22"` with standard encoding.

**Completed:**
- `src/metadata.rs`: `Metadata` type backed by `Vec<(String, String)>` (ordered, multi-value)
  - `insert`/`append`/`get`/`get_all` for ASCII metadata; `insert_bin`/`get_bin` for binary (-bin keys)
  - `from_request_headers`: filters pseudo-headers, grpc-*, content-type, te, host, etc.
  - `from_trailer_headers`: additionally filters grpc-status and grpc-message
  - `to_header_map`: converts back to http::HeaderMap for transmission
- `src/server.rs`: `UnaryHandlerFn` now `Fn(Bytes, Metadata) -> BoxFuture<...>`; request metadata
  extracted via `Metadata::from_request_headers` and passed to handler
- `src/client.rs`: `call_unary` takes `&Metadata` for request metadata, returns `(Resp, Metadata)`
  where trailing Metadata is extracted from response trailers
- 11 new unit tests in metadata.rs; 1 new integration test `request_metadata_reaches_handler`
- 65 total tests passing

---

### 2026-03-27 — Session 2 (continued): Deadline + cancellation (Module 7)

**Plan:**
Map `context.Context` deadline from grpc-go to `Option<Duration>` timeout on `call_unary`.
Client: wrap the entire RPC in `tokio::time::timeout`; if it fires, return `DeadlineExceeded`.
Also encode the timeout as `grpc-timeout` header on the wire so the server can enforce it.
Server: parse `grpc-timeout` header using grpc-go's unit format (`H/M/S/m/u/n`), wrap handler
invocation in `tokio::time::timeout`, return `DeadlineExceeded` on expiry.
Wire helpers `encode_timeout`/`decode_timeout` in `transport/mod.rs`.
Added `time` feature to tokio in both `[dependencies]` and `[dev-dependencies]`.

**Completed:**
- `transport::encode_timeout(Duration) -> String`: picks coarsest whole-number unit < 10^8
- `transport::decode_timeout(s: &str) -> Option<Duration>`: parses `<int><unit>` format
- `Channel::call_unary` gains `timeout: Option<Duration>` parameter; wraps RPC in
  `tokio::time::timeout` and encodes `grpc-timeout` header on the wire
- `dispatch_stream` parses `grpc-timeout` header and wraps handler call in deadline
- 10 new unit tests for timeout encoding/decoding; 2 new integration tests
  (`deadline_exceeded_on_slow_handler`, `call_with_timeout_succeeds_before_deadline`)
- 77 total tests passing

---

### 2026-03-27 — Session 2 (continued): Streaming RPCs (Module 8)

**Plan:**
Extend the server dispatch model with a `Handler` enum: `Unary(UnaryHandlerFn)` or
`Streaming(StreamingHandlerFn)`.  `StreamingHandlerFn = Arc<dyn Fn(ServerStream, Metadata) ->
BoxFuture<()>>` — the handler owns the `ServerStream` and drives it directly (read/write/close).
On the client side, add `StreamCall` wrapping `ClientStream` with typed `send_message<Req>`,
`recv_message<Resp>`, `close_send`, and `finish` methods.  `Channel::new_streaming_call` opens
the h2 stream without sending a request body, returning a `StreamCall`.  All four gRPC call
types (unary already done; server-streaming, client-streaming, bidi-streaming added here) are
now supported.  `MethodDesc.handler` changed from `UnaryHandlerFn` to `Handler` — all existing
callers updated to `Handler::Unary(...)`.

**Completed:**
- `Handler` enum (`Unary` / `Streaming`) in `server.rs`; `StreamingHandlerFn` type
- `dispatch_stream` routes to `dispatch_unary` (old path) or hands `ServerStream` to streaming handler
- `StreamCall` in `client.rs` with typed send/recv/close_send/finish
- `Channel::new_streaming_call(method, metadata, timeout) -> Result<StreamCall>`
- 3 new end-to-end integration tests: `server_streaming_multiple_responses`,
  `client_streaming_accumulate_names`, `bidi_streaming_echo`
- 80 total tests passing

---

### 2026-03-27 — Session 2 (continued): TLS (Module 9)

**Plan:**
Add `rustls` + `tokio-rustls` to support gRPC over TLS.  New `src/tls.rs` module provides
helper constructors for `ClientConfig` / `ServerConfig` with ALPN `h2` pre-configured.
`Channel::connect_tls` and `Server::serve_tls` are thin wrappers that do the TLS handshake
(via `TlsConnector` / `TlsAcceptor`) before passing the resulting stream to the existing
`ClientTransport::connect` / `ServerTransport::new` — the entire HTTP/2 and gRPC stack above
is unchanged.  Test uses `rcgen` to generate a self-signed cert for `localhost`; requires
calling `rustls::crypto::ring::default_provider().install_default()` once per test process.

**Completed:**
- `src/tls.rs`: `client_config_from_roots`, `server_config_from_cert`, `connector`, `acceptor`
- `Channel::connect_tls(addr, server_name, ClientConfig) -> Result<Self>`
- `Server::serve_tls(addr, ServerConfig) -> Result<(), Status>`
- `rcgen = "0.13"` added as dev-dependency for self-signed cert generation
- `rustls = "0.23"` + `tokio-rustls = "0.26"` in dependencies
- Integration test `tls_unary_round_trip` passing end-to-end
- 81 total tests passing

---

### 2026-03-27 — Session 2 (continued): Interceptors (Module 10)

**Plan:**
Add client and server unary interceptor chains matching grpc-go's `UnaryClientInterceptor` /
`UnaryServerInterceptor` pattern.  Work at the raw-bytes level (no prost generics in interceptors)
so the types are erasable.  `chain_server` / `chain_client` build the nested-closure chain
(outermost interceptor first, handler last).  Server interceptors wrap `dispatch_unary` and see
`(method, req_bytes, metadata, next)`.  Client interceptors sit in `call_unary_inner` around
the terminal `ClientNext` invoker; this required changing `Channel.transport` from
`Mutex<ClientTransport>` to `Arc<Mutex<ClientTransport>>` so the terminal closure could
capture it.

**Completed:**
- `src/interceptor.rs`: `UnaryServerInterceptor`, `UnaryClientInterceptor`, `ServerNext`,
  `ClientNext`, `chain_server`, `chain_client`
- `Server::add_interceptor(UnaryServerInterceptor)` — interceptors stored in `Vec`
  passed through to `dispatch_unary` via `Arc<Vec<...>>`
- `Channel::add_interceptor(UnaryClientInterceptor)` — interceptors wrapped around
  transport terminal in `call_unary_inner`
- 5 unit tests in `interceptor.rs` (chain ordering, short-circuit, metadata mutation)
- 2 integration tests: `client_interceptor_adds_metadata`, `server_interceptor_short_circuits`
- 88 total tests passing

---

### 2026-03-27 — Session 2 (continued): Interop test binary (Module 11)

**Plan:**
Create `src/bin/interop.rs` with inline prost message definitions matching
`grpc/testing/test.proto` (no proto compiler needed).  Binary supports both
`--mode=client` and `--mode=server`.  Test cases: `empty_unary`, `large_unary`,
`client_streaming`, `server_streaming`, `ping_pong`, `empty_stream`, `all`.
Server implements `grpc.testing.TestService` covering all four RPC call types.
In-process self-tests (`#[tokio::test]`) exercise each test case without needing
a running grpc-go reference server.

**Completed:**
- `src/bin/interop.rs`: 6 test cases + interop server + CLI argument parser
- All prost messages inline: `Empty`, `SimpleRequest/Response`, `Payload`,
  `StreamingInputCallRequest/Response`, `ResponseParameters`,
  `StreamingOutputCallRequest/Response`
- `prost = { features = ["derive"] }` moved to `[dependencies]` for binary use
- `tokio` `rt-multi-thread` + `macros` features added to `[dependencies]`
- 6 new in-process interop tests all passing
- 94 total tests passing (88 library + 6 interop)

**Next:** Run the interop binary against a live grpc-go reference server to validate
cross-implementation compatibility. Command:
  `cargo run --bin interop -- --mode=client --server_host=localhost --server_port=10000 --test_case=all`

---

### 2026-03-28 — Session 3: Cross-implementation interop validation

**Plan (written before any code):**
Run the grpc-go reference interop server (built in Session 2) and execute all 6 interop
test cases from the Rust client against it. Fix any failures found.

**Results:**
5/6 test cases passed immediately. `empty_stream` failed with:
  "missing grpc-status header in trailers"

**Root cause:**
`empty_stream` sends no messages and calls `FullDuplexCall` then immediately half-closes.
The grpc-go server responds with a "trailer-only" HTTP/2 response: a single HEADERS frame
with `END_STREAM=true` that contains both `:status: 200` and `grpc-status: 0`. There is
no separate TRAILERS frame. Our `recv_trailers()` called `RecvStream::trailers()` which
returned `None`, then `parse_grpc_status` failed on an empty `HeaderMap`.

**Fix:**
- Added `trailer_only_headers: Option<HeaderMap>` field to `ClientStream`
- In `recv_headers()`: if initial response headers contain `grpc-status`, cache them as
  `trailer_only_headers` and set `recv_done = true`
- In `recv_trailers()`: if `trailer_only_headers` is populated, return it immediately
  without calling `recv.trailers()`

**Other fix:**
- `interop.rs` main used `SocketAddr::parse()` for `--server_host=localhost`, which fails
  for DNS names. Replaced with `tokio::net::lookup_host().next()`.

**Outcome:** All 6 interop test cases PASS against grpc-go reference server:
  empty_unary, large_unary, client_streaming, server_streaming, ping_pong, empty_stream.
94 tests passing (unchanged).

---

### 2026-03-28 — Session 3 (continued): Basic retry policy (Module 12)

**Plan (written before any code):**
Add `RetryPolicy` in `src/retry.rs`: `max_attempts`, `initial_backoff`, `max_backoff`,
`backoff_multiplier`, `retryable_codes`. Wire into `Channel` via `with_retry_policy()`
builder. `call_unary` loops up to `max_attempts`, sleeps exponential backoff between
attempts, stops immediately on non-retryable codes, respects the outer `timeout`.
Not the full A6 spec (no per-method configs, no hedging, no retry throttling).

**Completed:**
- `src/retry.rs`: `RetryPolicy` struct, `is_retryable()`, `backoff_for_attempt()`
- `Channel::with_retry_policy(RetryPolicy) -> Self` builder (consumes, returns modified Channel)
- `call_unary` retry loop: inner async block runs up to `max_attempts` with backoff sleep;
  outer `tokio::time::timeout` wraps entire loop
- 3 unit tests in `retry.rs` (backoff math, retryable code check)
- 3 integration tests: `retry_succeeds_on_second_attempt`, `retry_exhausted_returns_last_error`,
  `non_retryable_code_not_retried` — uses `AtomicUsize` flaky handler
- 100 total tests passing (94 library + 6 interop)

---

### 2026-03-28 — Session 3 (continued): Examples (Module 13)

**Plan (written before any code):**
Add `examples/helloworld_server.rs` and `examples/helloworld_client.rs` mirroring
grpc-go's helloworld demo (unary only). Add `examples/route_guide_server.rs` and
`examples/route_guide_client.rs` mirroring grpc-go's route_guide demo, exercising all
four call types. Use inline prost message definitions (no proto compiler).

**Completed:**
- `examples/helloworld_server.rs`: Server implementing `helloworld.Greeter/SayHello`
- `examples/helloworld_client.rs`: Client calling `SayHello` with optional CLI name arg
- `examples/route_guide_server.rs`: Server with 4 RPC methods + static feature DB +
  haversine distance calculation
- `examples/route_guide_client.rs`: Client exercising GetFeature, ListFeatures, RecordRoute,
  RouteChat against the route_guide server
- `pub use crate::transport::ServerStream` added to `server.rs` for clean public API
- Fix: `find_feature<'a>` needed explicit lifetime annotation
- Fix: `Default` derive on `HelloReply` conflicted with prost's `Message` derive (removed)
- Smoke-tested both example pairs end-to-end; all four call types produce correct output
- 100 total tests passing (unchanged)

**Project status:** All CLAUDE.md success criteria met.

---

### 2026-03-28 — Session 3 (continued): Complete interop test suite (14 test cases)

**Plan (written before any code):**
Add 8 remaining interop test cases to cover the full non-auth gRPC interop spec:
status_code_and_message, special_status_message, unimplemented_method,
unimplemented_service, cancel_after_begin, cancel_after_first_response,
timeout_on_sleeping_server, custom_metadata.
Required infrastructure: stream cancellation, streaming deadline enforcement,
metadata echo in server responses.

**Completed:**

Infrastructure:
- `ClientStream::cancel()`: sends RST_STREAM(CANCEL) via `h2::SendStream::send_reset`
- `h2_error_to_status()`: RST_STREAM(CANCEL) → Cancelled; REFUSED_STREAM → Unavailable
- `StreamCall.deadline: Option<Instant>`: set from `timeout` in `new_streaming_call`
- `apply_deadline()` free function: wraps future in `tokio::time::timeout`; also converts
  CANCELLED → DEADLINE_EXCEEDED when deadline has elapsed (handles race where grpc-go
  sends RST_STREAM(CANCEL) due to `grpc-timeout` expiry before our local timer fires)
- `Code: TryFrom<u32>`: needed to convert EchoStatus.code from protobuf to gRPC status code

Server changes:
- `UnaryCall` handler changed to `Handler::Streaming` to support echoing metadata in
  initial response headers AND trailing headers (required for custom_metadata test)
- `interval_us` delay in `StreamingOutputCall` and `FullDuplexCall` handlers
- `SimpleRequest`: added `EchoStatus` message type and `response_status` field (tag 7)

Failures found and fixed during cross-implementation testing:
- `timeout_on_sleeping_server`: initially got CANCELLED instead of DEADLINE_EXCEEDED
  because grpc-go sends RST_STREAM(CANCEL) when `grpc-timeout` expires on server side.
  Fixed by checking deadline expiry after CANCELLED in `apply_deadline`.

**Outcome:** All 14 interop test cases PASS against grpc-go reference server.
108 total tests passing (94 library + 14 interop).

---

