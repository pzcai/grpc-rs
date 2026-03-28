//! gRPC transport layer: HTTP/2 framing, stream lifecycle, and flow control.
//!
//! This module provides:
//! - [`ServerTransport`]: manages a single server-side HTTP/2 connection
//! - [`ServerStream`]: one RPC within that connection (server side)
//! - [`ClientTransport`]: manages a single client-side HTTP/2 connection
//! - [`ClientStream`]: one RPC within that connection (client side)
//!
//! Design notes:
//! - The 5-byte gRPC frame header is NOT constructed here; the codec layer above
//!   is responsible. The transport sends and receives raw `Bytes`, which happen to
//!   contain the gRPC-framed payload.
//! - DATA frames from the peer arrive in arbitrary-sized chunks. Both `ServerStream`
//!   and `ClientStream` buffer incoming bytes and extract complete gRPC frames using
//!   the 5-byte length prefix.
//! - Flow control: `release_capacity` is called after every read, returning HTTP/2
//!   window credit to the peer.

pub mod client;
pub mod server;

pub use client::{ClientStream, ClientTransport};
pub use server::{ServerStream, ServerTransport};

use std::time::Duration;

use bytes::{Bytes, BytesMut};

use crate::codec::HEADER_LEN;
use crate::status::Status;

// ── gRPC timeout wire encoding ────────────────────────────────────────────────

/// Encode a `Duration` as a gRPC `grpc-timeout` header value.
///
/// Format: `<integer><unit>` where unit is one of `H`, `M`, `S`, `m`, `u`, `n`
/// (hours, minutes, seconds, milliseconds, microseconds, nanoseconds).
/// We choose the coarsest unit that fits in a u64 with at most 8 digits
/// (grpc-go uses the same strategy: pick the largest unit where the value
/// is a whole number and < 10^8).
pub(crate) fn encode_timeout(d: Duration) -> String {
    let nanos = d.as_nanos();
    // Largest unit first: hours, minutes, seconds, millis, micros, nanos
    let units: &[(u128, char)] = &[
        (3_600_000_000_000, 'H'),
        (60_000_000_000, 'M'),
        (1_000_000_000, 'S'),
        (1_000_000, 'm'),
        (1_000, 'u'),
        (1, 'n'),
    ];
    for &(unit_nanos, suffix) in units {
        if nanos % unit_nanos == 0 {
            let value = nanos / unit_nanos;
            if value < 100_000_000 {
                return format!("{value}{suffix}");
            }
        }
    }
    // Fallback: nanoseconds (always exact)
    format!("{}n", nanos)
}

/// Decode a gRPC `grpc-timeout` header value into a `Duration`.
///
/// Returns `None` if the format is unrecognised or the integer overflows.
pub(crate) fn decode_timeout(s: &str) -> Option<Duration> {
    if s.is_empty() {
        return None;
    }
    let suffix = s.chars().last()?;
    let num_str = &s[..s.len() - 1];
    let value: u64 = num_str.parse().ok()?;
    match suffix {
        'H' => Some(Duration::from_secs(value * 3600)),
        'M' => Some(Duration::from_secs(value * 60)),
        'S' => Some(Duration::from_secs(value)),
        'm' => Some(Duration::from_millis(value)),
        'u' => Some(Duration::from_micros(value)),
        'n' => Some(Duration::from_nanos(value)),
        _ => None,
    }
}

/// Try to extract one complete gRPC frame from the buffer.
///
/// A complete frame is HEADER_LEN (5) bytes of header followed by the
/// payload length encoded in bytes 1-4 of the header.
///
/// Returns `Ok(Some(frame))` when a full frame is available, `Ok(None)` when
/// more bytes are needed, or `Err` for a malformed header.
pub(crate) fn try_extract_frame(buf: &mut BytesMut) -> Result<Option<Bytes>, Status> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let msg_len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    let total = HEADER_LEN + msg_len;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some(buf.split_to(total).freeze()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;
    use std::time::Duration;

    // ── timeout encoding ──────────────────────────────────────────────────────

    #[test]
    fn encode_timeout_hours() {
        assert_eq!(encode_timeout(Duration::from_secs(7200)), "2H");
    }

    #[test]
    fn encode_timeout_minutes() {
        assert_eq!(encode_timeout(Duration::from_secs(120)), "2M");
    }

    #[test]
    fn encode_timeout_seconds() {
        assert_eq!(encode_timeout(Duration::from_secs(5)), "5S");
    }

    #[test]
    fn encode_timeout_millis() {
        assert_eq!(encode_timeout(Duration::from_millis(250)), "250m");
    }

    #[test]
    fn encode_timeout_micros() {
        assert_eq!(encode_timeout(Duration::from_micros(500)), "500u");
    }

    #[test]
    fn encode_timeout_nanos() {
        assert_eq!(encode_timeout(Duration::from_nanos(100)), "100n");
    }

    #[test]
    fn decode_timeout_roundtrip_millis() {
        let d = Duration::from_millis(500);
        let encoded = encode_timeout(d);
        assert_eq!(decode_timeout(&encoded), Some(d));
    }

    #[test]
    fn decode_timeout_roundtrip_seconds() {
        let d = Duration::from_secs(30);
        let encoded = encode_timeout(d);
        assert_eq!(decode_timeout(&encoded), Some(d));
    }

    #[test]
    fn decode_timeout_bad_suffix_returns_none() {
        assert_eq!(decode_timeout("100x"), None);
    }

    #[test]
    fn decode_timeout_empty_returns_none() {
        assert_eq!(decode_timeout(""), None);
    }

    #[test]
    fn extract_frame_empty_buf() {
        let mut buf = BytesMut::new();
        assert!(try_extract_frame(&mut buf).unwrap().is_none());
    }

    #[test]
    fn extract_frame_incomplete_header() {
        let mut buf = BytesMut::from(&[0x00, 0x00][..]);
        assert!(try_extract_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), 2, "buffer must not be consumed");
    }

    #[test]
    fn extract_frame_header_only_no_body_yet() {
        // Header says 10 bytes, but we only have the 5-byte header
        let mut buf = BytesMut::new();
        buf.put_u8(0x00);
        buf.put_u32(10);
        assert!(try_extract_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), 5, "partial frame must not be consumed");
    }

    #[test]
    fn extract_frame_exact_frame() {
        let payload = b"hello";
        let mut buf = BytesMut::new();
        buf.put_u8(0x00);
        buf.put_u32(payload.len() as u32);
        buf.put_slice(payload);

        let frame = try_extract_frame(&mut buf).unwrap().unwrap();
        assert_eq!(frame.len(), HEADER_LEN + payload.len());
        assert!(buf.is_empty(), "consumed frame must be removed from buf");
    }

    #[test]
    fn extract_frame_leaves_remainder() {
        // Two frames concatenated; only the first should be extracted
        let p1 = b"first";
        let p2 = b"second";
        let mut buf = BytesMut::new();
        buf.put_u8(0x00);
        buf.put_u32(p1.len() as u32);
        buf.put_slice(p1);
        buf.put_u8(0x00);
        buf.put_u32(p2.len() as u32);
        buf.put_slice(p2);

        let frame1 = try_extract_frame(&mut buf).unwrap().unwrap();
        assert_eq!(&frame1[HEADER_LEN..], p1);

        let frame2 = try_extract_frame(&mut buf).unwrap().unwrap();
        assert_eq!(&frame2[HEADER_LEN..], p2);

        assert!(buf.is_empty());
    }
}
