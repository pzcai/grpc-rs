//! gRPC codec layer: wire framing, compression, and message serialization.
//!
//! Every gRPC message on the wire is wrapped in a 5-byte length-prefix frame:
//!
//! ```text
//! Byte 0:    compression flag  (0x00 = none, 0x01 = compressed)
//! Bytes 1-4: payload length    (big-endian u32)
//! Bytes 5+:  payload
//! ```
//!
//! This module closely follows the behavior documented in grpc-go's `rpc_util.go`
//! and `encoding/` package.

use bytes::{BufMut, Bytes, BytesMut};

use crate::status::Status;

pub mod gzip;

// ── Constants ────────────────────────────────────────────────────────────────

/// Number of bytes in the gRPC message frame header.
pub const HEADER_LEN: usize = 5;

/// Default maximum receive message size: 4 MiB, matching grpc-go's default
/// (`defaultClientMaxRecvMsgSize` / `defaultServerMaxRecvMsgSize`).
pub const DEFAULT_MAX_RECV_SIZE: usize = 4 * 1024 * 1024;

const COMPRESSION_NONE: u8 = 0x00;
const COMPRESSION_MADE: u8 = 0x01;

// ── Compressor trait ─────────────────────────────────────────────────────────

/// A compression algorithm that can compress and decompress byte slices.
///
/// Implementations must be `Send + Sync` so they can be shared across async tasks.
/// The `name()` string is used verbatim in the `grpc-encoding` HTTP/2 header.
///
/// Mirrors grpc-go's `encoding.Compressor` interface.
pub trait Compressor: Send + Sync + 'static {
    /// Algorithm name used in the `grpc-encoding` header (e.g. `"gzip"`).
    fn name(&self) -> &'static str;

    /// Compress `input` and return the compressed bytes.
    fn compress(&self, input: &[u8]) -> Result<Vec<u8>, Status>;

    /// Decompress `input` and return the original bytes.
    fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, Status>;
}

// ── Frame encoding ───────────────────────────────────────────────────────────

/// Serialize a prost message and wrap it in a gRPC data frame.
///
/// If `compressor` is provided **and** the serialized payload is non-empty,
/// the payload is compressed. Zero-length payloads are never compressed
/// (matches grpc-go's `compress()` which skips compression when `len == 0`).
pub fn encode_message<M: prost::Message>(
    msg: &M,
    compressor: Option<&dyn Compressor>,
) -> Result<Bytes, Status> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf)
        .map_err(|e| Status::internal(format!("proto encode: {e}")))?;
    encode_raw(&buf, compressor)
}

/// Wrap already-serialized bytes in a gRPC data frame.
///
/// This is the lower-level counterpart to `encode_message`, useful when the
/// transport layer already has raw bytes (e.g. forwarding a message).
pub fn encode_raw(
    payload: &[u8],
    compressor: Option<&dyn Compressor>,
) -> Result<Bytes, Status> {
    let (flag, body): (u8, Vec<u8>) = match compressor {
        Some(c) if !payload.is_empty() => (COMPRESSION_MADE, c.compress(payload)?),
        _ => (COMPRESSION_NONE, payload.to_vec()),
    };

    let len = body.len();
    if len > u32::MAX as usize {
        return Err(Status::resource_exhausted(format!(
            "message too large: {len} bytes (max {})",
            u32::MAX
        )));
    }

    let mut out = BytesMut::with_capacity(HEADER_LEN + len);
    out.put_u8(flag);
    out.put_u32(len as u32);
    out.put_slice(&body);
    Ok(out.freeze())
}

// ── Frame decoding ───────────────────────────────────────────────────────────

/// Decode a gRPC data frame and deserialize it into a prost message.
///
/// `max_recv_size` is enforced twice: once on the wire payload length, and again
/// on the decompressed payload length (matches grpc-go's double-check).
pub fn decode_message<M: prost::Message + Default>(
    frame: &[u8],
    decompressor: Option<&dyn Compressor>,
    max_recv_size: usize,
) -> Result<M, Status> {
    let payload = decode_raw(frame, decompressor, max_recv_size)?;
    M::decode(payload.as_slice())
        .map_err(|e| Status::internal(format!("proto decode: {e}")))
}

/// Decode a gRPC data frame and return the raw payload bytes.
///
/// Validates the frame header and applies decompression if indicated by the
/// compression flag.
pub fn decode_raw(
    frame: &[u8],
    decompressor: Option<&dyn Compressor>,
    max_recv_size: usize,
) -> Result<Vec<u8>, Status> {
    if frame.len() < HEADER_LEN {
        return Err(Status::internal(format!(
            "frame too short: {} bytes (need {})",
            frame.len(),
            HEADER_LEN
        )));
    }

    let flag = frame[0];
    let length = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;

    // Enforce max receive size on wire payload (first check)
    if length > max_recv_size {
        return Err(Status::resource_exhausted(format!(
            "wire payload {length} bytes exceeds max recv size {max_recv_size}"
        )));
    }

    if frame.len() < HEADER_LEN + length {
        return Err(Status::internal(format!(
            "frame truncated: header says {length} bytes but only {} available",
            frame.len() - HEADER_LEN
        )));
    }

    let body = &frame[HEADER_LEN..HEADER_LEN + length];

    match flag {
        COMPRESSION_NONE => Ok(body.to_vec()),

        COMPRESSION_MADE => {
            // grpc-go: if compressed flag is set but decompressor is absent, return
            // Internal (client) or Unimplemented (server). We use Internal here and
            // will thread client/server context through when the transport layer is built.
            let dc = decompressor.ok_or_else(|| {
                Status::internal("compressed flag set but no decompressor available")
            })?;

            let decompressed = dc.decompress(body)?;

            // Enforce max receive size on decompressed size (second check)
            if decompressed.len() > max_recv_size {
                return Err(Status::resource_exhausted(format!(
                    "decompressed payload {} bytes exceeds max recv size {max_recv_size}",
                    decompressed.len()
                )));
            }

            Ok(decompressed)
        }

        other => Err(Status::internal(format!(
            "unexpected compression flag: 0x{other:02x} (must be 0x00 or 0x01)"
        ))),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::gzip::GzipCompressor;
    use crate::status::Code;

    // Minimal prost message for testing without a .proto file.
    // Fields: value (string, tag 1)
    #[derive(Clone, PartialEq, prost::Message)]
    struct Ping {
        #[prost(string, tag = "1")]
        value: String,
    }

    // ── encode_raw / decode_raw ──────────────────────────────────────────────

    #[test]
    fn encode_raw_no_compression_frame_layout() {
        let payload = b"hello";
        let frame = encode_raw(payload, None).unwrap();
        // [0x00, 0x00, 0x00, 0x00, 0x05, 'h', 'e', 'l', 'l', 'o']
        assert_eq!(frame[0], COMPRESSION_NONE);
        assert_eq!(u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]), 5);
        assert_eq!(&frame[5..], b"hello");
    }

    #[test]
    fn encode_raw_with_gzip_sets_compression_flag() {
        let c = GzipCompressor::new();
        let payload = b"hello gzip";
        let frame = encode_raw(payload, Some(&c)).unwrap();
        assert_eq!(frame[0], COMPRESSION_MADE);
    }

    #[test]
    fn encode_raw_empty_payload_never_compressed() {
        // grpc-go: "if the marshaled payload is empty (len == 0), compression is skipped"
        let c = GzipCompressor::new();
        let frame = encode_raw(b"", Some(&c)).unwrap();
        assert_eq!(frame[0], COMPRESSION_NONE, "zero-length payload must not be compressed");
        assert_eq!(u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]), 0);
    }

    #[test]
    fn decode_raw_round_trip_no_compression() {
        let payload = b"round trip test";
        let frame = encode_raw(payload, None).unwrap();
        let decoded = decode_raw(&frame, None, DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn decode_raw_round_trip_gzip() {
        let c = GzipCompressor::new();
        let payload = b"compress me please";
        let frame = encode_raw(payload, Some(&c)).unwrap();
        let decoded = decode_raw(&frame, Some(&c), DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn decode_raw_frame_too_short_returns_internal() {
        let result = decode_raw(b"\x00\x00\x00", None, DEFAULT_MAX_RECV_SIZE);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, Code::Internal);
    }

    #[test]
    fn decode_raw_truncated_body_returns_internal() {
        // Header says 10 bytes, but only 3 provided
        let mut frame = vec![0x00, 0x00, 0x00, 0x00, 0x0A]; // length = 10
        frame.extend_from_slice(b"abc"); // only 3 bytes
        let result = decode_raw(&frame, None, DEFAULT_MAX_RECV_SIZE);
        assert_eq!(result.unwrap_err().code, Code::Internal);
    }

    #[test]
    fn decode_raw_wire_size_exceeds_max_returns_resource_exhausted() {
        // Header says 100 bytes, max is 50
        let mut frame = vec![0x00, 0x00, 0x00, 0x00, 0x64]; // length = 100
        frame.extend(vec![0u8; 100]);
        let result = decode_raw(&frame, None, 50);
        assert_eq!(result.unwrap_err().code, Code::ResourceExhausted);
    }

    #[test]
    fn decode_raw_invalid_compression_flag_returns_internal() {
        // Flag = 0x02, which is not defined
        let frame = vec![0x02, 0x00, 0x00, 0x00, 0x00];
        let result = decode_raw(&frame, None, DEFAULT_MAX_RECV_SIZE);
        assert_eq!(result.unwrap_err().code, Code::Internal);
    }

    #[test]
    fn decode_raw_compressed_flag_no_decompressor_returns_internal() {
        // grpc-go: compressed flag set but no decompressor → Internal (client side)
        let c = GzipCompressor::new();
        let frame = encode_raw(b"test", Some(&c)).unwrap();
        assert_eq!(frame[0], COMPRESSION_MADE);
        let result = decode_raw(&frame, None, DEFAULT_MAX_RECV_SIZE);
        assert_eq!(result.unwrap_err().code, Code::Internal);
    }

    #[test]
    fn decode_raw_decompressed_size_exceeds_max_returns_resource_exhausted() {
        // Compress a payload that decompresses to more than max_recv_size.
        // We set max_recv_size to something small after compression.
        let c = GzipCompressor::new();
        let payload = vec![b'A'; 100];
        let frame = encode_raw(&payload, Some(&c)).unwrap();
        // Allow the compressed wire size (which is ~30 bytes) but reject decompressed (100 bytes)
        let result = decode_raw(&frame, Some(&c), 50);
        assert_eq!(result.unwrap_err().code, Code::ResourceExhausted);
    }

    // ── encode_message / decode_message ─────────────────────────────────────

    #[test]
    fn message_round_trip_no_compression() {
        let msg = Ping { value: "hello proto".into() };
        let frame = encode_message(&msg, None).unwrap();
        let decoded: Ping = decode_message(&frame, None, DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn message_round_trip_gzip() {
        let c = GzipCompressor::new();
        let msg = Ping { value: "compress this proto message".into() };
        let frame = encode_message(&msg, Some(&c)).unwrap();
        let decoded: Ping = decode_message(&frame, Some(&c), DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_message_bad_proto_returns_internal() {
        // Valid frame header but garbage proto bytes
        let frame = encode_raw(b"\xff\xff\xff", None).unwrap();
        let result: Result<Ping, Status> = decode_message(&frame, None, DEFAULT_MAX_RECV_SIZE);
        assert_eq!(result.unwrap_err().code, Code::Internal);
    }
}
