# Changelog

## How to use this file
- Add an entry after every meaningful unit of work
- Record failed approaches with the reason they failed
- Future sessions: read this file first, then continue from Current Status

---

## Current status
Module 1 (Codec layer) — IN PROGRESS (session 1).

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
