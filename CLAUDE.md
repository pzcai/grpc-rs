# Project: grpc-go → Rust port

## Objective
Port the Go gRPC implementation at https://github.com/grpc/grpc-go to idiomatic Rust.
The target crate is `grpc-rs` (name tentative). The port must be functionally equivalent
to grpc-go's core, verified against the same interop test suite used by the gRPC project.

## Success criteria
1. All gRPC interop test cases pass against a reference Go server/client
   (https://github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md)
2. The four call types work correctly: unary, server streaming, client streaming,
   bidirectional streaming
3. TLS + plaintext both supported
4. Metadata (headers/trailers) round-trips correctly
5. Deadline/cancellation propagates correctly
6. Basic interceptor (middleware) API is implemented on both client and server side
7. `cargo test` passes with zero failures at every commit

## Out of scope (for now)
- xDS / load balancing plugins
- ORCA
- Admin services
- Channelz
- Retry policy (implement basic, not full A6 spec)

## Architecture decisions (update this section as you go)
- Async runtime: tokio
- HTTP/2: h2 crate (wraps hyper/tokio)
- Protobuf codec: prost
- TLS: rustls (not native-tls)
- Project layout:
  - `grpc-rs/`          — core library crate
  - `grpc-rs-interop/`  — interop test binary (client + server)
  - `grpc-rs-examples/` — helloworld and route_guide examples

## Working rules
- Read CHANGELOG.md at the start of every session and continue from where you left off.
- Update CHANGELOG.md after every meaningful unit of work (see format below).
- Run `cargo test --workspace` before every commit. Never commit code that breaks
  currently passing tests.
- Commit and push after every meaningful unit of work. Commit message format:
  `[module] short description` e.g. `[transport] implement HTTP/2 frame reader`
- When you hit a dead end, record WHY in CHANGELOG.md before abandoning the approach.
  Future sessions must not re-attempt approaches already marked as failed.
- When the Rust borrow checker rejects an approach after 3+ attempts, step back,
  re-read the Go source for that module, and design a different ownership model.
  Record the new design in CHANGELOG.md before implementing.
- Use the grpc-go source as the authoritative reference for behavior, not for structure.
  Go's interface-based polymorphism maps to Rust traits; Go goroutines map to tokio tasks;
  Go channels map to tokio mpsc/oneshot. Do not do a line-by-line transliteration.

## Porting order (update as needed)
Work through modules in this order. Each module should have passing unit tests before
moving to the next.

1. [ ] Codec layer — protobuf encode/decode, compression (identity + gzip)
2. [ ] Transport layer — HTTP/2 framing via h2, stream lifecycle, flow control
3. [ ] Server core — listener, connection accept loop, stream dispatch
4. [ ] Client core — channel, subchannel, connection management, call creation
5. [ ] Unary RPC — end-to-end, plaintext
6. [ ] Metadata — headers and trailers
7. [ ] Deadline + cancellation — context equivalent via tokio CancellationToken
8. [ ] Streaming RPCs — server, client, bidirectional
9. [ ] TLS — rustls integration
10. [ ] Interceptors — client and server middleware chain
11. [ ] Interop test binary — run against grpc-go reference

## Test oracle
The primary oracle is the gRPC interop test suite. Run a grpc-go interop server
locally and test grpc-rs against it, and vice versa.

Secondary oracle: every module has its own unit tests. When Go behavior is ambiguous,
write a small Go program to observe the behavior, then encode that observation as a
Rust test.

## Reference materials
- grpc-go source: https://github.com/grpc/grpc-go
- gRPC core spec: https://github.com/grpc/grpc/tree/master/doc
- gRPC interop tests: https://github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md
- h2 crate docs: https://docs.rs/h2
- prost crate docs: https://docs.rs/prost
- tokio docs: https://docs.rs/tokio
- rustls docs: https://docs.rs/rustls
