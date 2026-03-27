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

use bytes::{Bytes, BytesMut};

use crate::codec::HEADER_LEN;
use crate::status::Status;

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
