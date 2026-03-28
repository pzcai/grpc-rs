# Changelog

## How to use this file
- Add an entry after every meaningful unit of work
- Record failed approaches with the reason they failed
- Future sessions: read this file first, then continue from Current Status

---

## Current status
Modules 1–5 COMPLETE (codec, transport, server, client, unary RPC). Begin Module 6 (Metadata).

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

**Next session — Module 6:** Metadata (headers and trailers). The transport layer already
passes `HeaderMap` through; Module 6 formalizes the `Metadata` abstraction, adds metadata
forwarding through `Channel::call_unary`, and makes the server handler receive and return
metadata. Key behaviors from grpc-go: binary metadata keys end in `-bin` and are base64
encoded; ASCII metadata is passed verbatim; filtered headers (`:*`, `host`, `connection`,
`grpc-*` except `grpc-encoding`, `grpc-accept-encoding`, `grpc-timeout`) are removed.
