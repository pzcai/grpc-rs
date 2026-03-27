//! Client-side HTTP/2 transport.
//!
//! [`ClientTransport`] wraps a single client HTTP/2 connection and creates
//! [`ClientStream`]s for outgoing RPCs.
//!
//! The h2 `Connection` future must be polled continuously to drive the connection.
//! `ClientTransport::connect` returns it separately so the caller can spawn it:
//! ```ignore
//! let (transport, conn_fut) = ClientTransport::connect(tcp_stream).await?;
//! tokio::spawn(conn_fut);
//! ```
//! This keeps the library free of hard `tokio::spawn` dependencies.

use bytes::{Bytes, BytesMut};
use http::HeaderMap;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::status::{Code, Status};
use super::try_extract_frame;

// ── ClientTransport ───────────────────────────────────────────────────────────

/// Client-side HTTP/2 transport for a single connection.
pub struct ClientTransport {
    sender: h2::client::SendRequest<Bytes>,
}

impl ClientTransport {
    /// Perform the HTTP/2 client handshake on `io` and return the transport
    /// plus the connection future that drives framing.
    ///
    /// The caller MUST spawn or `.await` the returned future:
    /// ```ignore
    /// let (transport, conn_fut) = ClientTransport::connect(stream).await?;
    /// tokio::spawn(conn_fut);
    /// ```
    pub async fn connect<T: AsyncRead + AsyncWrite + Unpin>(
        io: T,
    ) -> Result<(Self, impl std::future::Future<Output = ()>), Status> {
        let (sender, conn) = h2::client::handshake(io)
            .await
            .map_err(|e| Status::internal(format!("HTTP/2 client handshake: {e}")))?;

        let conn_fut = async move {
            // Drive the connection; errors are ignored here because the
            // ClientTransport will surface them as stream errors when callers
            // try to create new streams or send data.
            conn.await.ok();
        };

        Ok((ClientTransport { sender }, conn_fut))
    }

    /// Create a new RPC stream to `method`.
    ///
    /// `method` is the full RPC path, e.g. `/helloworld.Greeter/SayHello`.
    /// `extra_headers` is user-supplied request metadata (will be appended after
    /// the required gRPC headers).
    ///
    /// Returns `Err` if the connection has been closed or at capacity.
    pub fn new_stream(
        &mut self,
        method: &str,
        extra_headers: &HeaderMap,
    ) -> Result<ClientStream, Status> {
        let mut builder = http::Request::builder()
            .method("POST")
            .uri(method)
            .header("content-type", "application/grpc")
            .header("te", "trailers");

        for (name, value) in extra_headers.iter() {
            builder = builder.header(name, value);
        }

        let request = builder
            .body(())
            .map_err(|e| Status::internal(format!("build request: {e}")))?;

        let (response_fut, send) = self
            .sender
            .send_request(request, false)
            .map_err(|e| Status::internal(format!("send_request: {e}")))?;

        Ok(ClientStream {
            send,
            response_fut: Some(response_fut),
            recv: None,
            buf: BytesMut::new(),
            recv_done: false,
        })
    }
}

// ── ClientStream ──────────────────────────────────────────────────────────────

/// A single gRPC RPC stream on the client side.
pub struct ClientStream {
    send: h2::SendStream<Bytes>,
    /// Pending response from the server. Consumed by the first `recv_headers` call.
    response_fut: Option<h2::client::ResponseFuture>,
    /// Response body (available after `recv_headers` has been called).
    recv: Option<h2::RecvStream>,
    /// Buffered bytes for frame reassembly.
    buf: BytesMut,
    /// Set when all DATA frames from the server have been consumed.
    recv_done: bool,
}

impl ClientStream {
    /// Send a gRPC data frame to the server.
    ///
    /// `data` must be the already-framed bytes from [`crate::codec::encode_raw`]
    /// or [`crate::codec::encode_message`].
    ///
    /// Set `end_of_stream = true` on the last message (unary RPCs always use
    /// `true`). Setting it to `false` allows sending additional messages
    /// (client-streaming RPCs).
    pub fn send_message(&mut self, data: Bytes, end_of_stream: bool) -> Result<(), Status> {
        self.send
            .send_data(data, end_of_stream)
            .map_err(|e| Status::internal(format!("send_data: {e}")))
    }

    /// Finish the request stream without sending additional data (END_STREAM only).
    ///
    /// Not needed if the last call to `send_message` used `end_of_stream = true`.
    pub fn finish_send(&mut self) -> Result<(), Status> {
        self.send
            .send_data(Bytes::new(), true)
            .map_err(|e| Status::internal(format!("finish_send: {e}")))
    }

    /// Wait for and return the initial response headers from the server.
    ///
    /// Must be called before [`recv_message`] or [`recv_trailers`].
    /// May only be called once; subsequent calls return the cached headers.
    pub async fn recv_headers(&mut self) -> Result<HeaderMap, Status> {
        if let Some(fut) = self.response_fut.take() {
            let response = fut
                .await
                .map_err(|e| Status::internal(format!("await response headers: {e}")))?;

            // Validate :status and content-type
            if response.status() != 200 {
                return Err(Status::internal(format!(
                    "unexpected HTTP status: {}",
                    response.status()
                )));
            }

            let (parts, body) = response.into_parts();

            if let Some(ct) = parts.headers.get("content-type") {
                if !ct
                    .to_str()
                    .unwrap_or("")
                    .starts_with("application/grpc")
                {
                    return Err(Status::internal(format!(
                        "unexpected content-type: {}",
                        ct.to_str().unwrap_or("<non-utf8>")
                    )));
                }
            }

            self.recv = Some(body);
            Ok(parts.headers)
        } else {
            // Headers already consumed; this is a programming error but return
            // an empty map rather than panic.
            Ok(HeaderMap::new())
        }
    }

    /// Read the next gRPC message from the server.
    ///
    /// Returns the raw frame bytes (5-byte header + payload). Pass these to
    /// [`crate::codec::decode_raw`] or [`crate::codec::decode_message`].
    ///
    /// Returns `Ok(None)` when the server has sent END_STREAM (no more messages).
    ///
    /// `recv_headers` must have been called first.
    pub async fn recv_message(&mut self) -> Result<Option<Bytes>, Status> {
        let recv = self
            .recv
            .as_mut()
            .ok_or_else(|| Status::internal("recv_headers must be called before recv_message"))?;

        loop {
            if let Some(frame) = try_extract_frame(&mut self.buf)? {
                return Ok(Some(frame));
            }

            if self.recv_done {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(Status::internal("unexpected EOF: incomplete gRPC message"))
                };
            }

            match recv.data().await {
                Some(Ok(data)) => {
                    let n = data.len();
                    self.buf.extend_from_slice(&data);
                    recv.flow_control()
                        .release_capacity(n)
                        .map_err(|e| Status::internal(format!("release_capacity: {e}")))?;
                }
                Some(Err(e)) => return Err(Status::internal(format!("recv data frame: {e}"))),
                None => {
                    self.recv_done = true;
                }
            }
        }
    }

    /// Read trailing headers and derive the RPC completion status.
    ///
    /// Discards any remaining data frames before reading trailers (tolerates
    /// partial consumption of a streaming response).
    ///
    /// Returns `(Status, extra_trailer_headers)`.
    /// `recv_headers` must have been called first.
    pub async fn recv_trailers(&mut self) -> Result<(Status, HeaderMap), Status> {
        let recv = self
            .recv
            .as_mut()
            .ok_or_else(|| Status::internal("recv_headers must be called before recv_trailers"))?;

        // Drain remaining DATA frames.
        while !self.recv_done {
            match recv.data().await {
                Some(Ok(data)) => {
                    let n = data.len();
                    recv.flow_control()
                        .release_capacity(n)
                        .map_err(|e| Status::internal(format!("drain release_capacity: {e}")))?;
                }
                Some(Err(e)) => {
                    return Err(Status::internal(format!("drain data: {e}")));
                }
                None => {
                    self.recv_done = true;
                }
            }
        }

        let trailers = recv
            .trailers()
            .await
            .map_err(|e| Status::internal(format!("recv trailers: {e}")))?
            .unwrap_or_default();

        let status = parse_grpc_status(&trailers)?;
        Ok((status, trailers))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse `grpc-status` (and optionally `grpc-message`) from a trailer `HeaderMap`.
fn parse_grpc_status(trailers: &HeaderMap) -> Result<Status, Status> {
    let code_str = trailers
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Status::internal("missing grpc-status header in trailers"))?;

    let code_u32: u32 = code_str
        .parse()
        .map_err(|_| Status::internal(format!("non-numeric grpc-status: {code_str}")))?;

    let code = code_from_u32(code_u32)?;

    let message = trailers
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode_grpc_message)
        .unwrap_or_default();

    Ok(Status::new(code, message))
}

fn code_from_u32(n: u32) -> Result<Code, Status> {
    Ok(match n {
        0 => Code::Ok,
        1 => Code::Cancelled,
        2 => Code::Unknown,
        3 => Code::InvalidArgument,
        4 => Code::DeadlineExceeded,
        5 => Code::NotFound,
        6 => Code::AlreadyExists,
        7 => Code::PermissionDenied,
        8 => Code::ResourceExhausted,
        9 => Code::FailedPrecondition,
        10 => Code::Aborted,
        11 => Code::OutOfRange,
        12 => Code::Unimplemented,
        13 => Code::Internal,
        14 => Code::Unavailable,
        15 => Code::DataLoss,
        16 => Code::Unauthenticated,
        _ => return Err(Status::internal(format!("unknown grpc-status code: {n}"))),
    })
}

/// Decode a percent-encoded gRPC status message.
fn percent_decode_grpc_message(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                from_hex(bytes[i + 1]),
                from_hex(bytes[i + 2]),
            ) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Code;
    use crate::transport::server::ServerTransport;

    // ── percent_decode_grpc_message ──────────────────────────────────────────

    #[test]
    fn decode_plain_ascii_unchanged() {
        assert_eq!(percent_decode_grpc_message("hello"), "hello");
    }

    #[test]
    fn decode_percent_encoded_percent() {
        assert_eq!(percent_decode_grpc_message("50%25 off"), "50% off");
    }

    #[test]
    fn decode_non_printable() {
        assert_eq!(percent_decode_grpc_message("%01"), "\x01");
    }

    #[test]
    fn decode_utf8_sequence() {
        // %C3%A9 → é
        assert_eq!(percent_decode_grpc_message("%C3%A9"), "é");
    }

    // ── parse_grpc_status ────────────────────────────────────────────────────

    #[test]
    fn parse_ok_status() {
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        let status = parse_grpc_status(&trailers).unwrap();
        assert_eq!(status.code, Code::Ok);
        assert_eq!(status.message, "");
    }

    #[test]
    fn parse_error_with_message() {
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "12".parse().unwrap());
        trailers.insert("grpc-message", "not%20implemented".parse().unwrap());
        let status = parse_grpc_status(&trailers).unwrap();
        assert_eq!(status.code, Code::Unimplemented);
        assert_eq!(status.message, "not implemented");
    }

    #[test]
    fn parse_missing_grpc_status_returns_internal() {
        let trailers = HeaderMap::new();
        let result = parse_grpc_status(&trailers);
        assert_eq!(result.unwrap_err().code, Code::Internal);
    }

    // ── ClientTransport + ServerTransport integration ────────────────────────

    /// Full unary RPC round-trip using both transport implementations.
    #[tokio::test]
    async fn client_server_unary_round_trip() {
        use crate::codec;
        use tokio::io::duplex;

        let (server_io, client_io) = duplex(64 * 1024);

        // ── Server ────────────────────────────────────────────────────────────
        let server = tokio::spawn(async move {
            let mut transport = ServerTransport::new(server_io).await.unwrap();
            let mut stream = transport.accept().await.unwrap();

            let frame = stream.recv_message().await.unwrap().unwrap();
            let payload: Vec<u8> =
                codec::decode_raw(&frame, None, codec::DEFAULT_MAX_RECV_SIZE).unwrap();
            assert_eq!(payload, b"request");

            // Echo "response"
            let resp_frame = codec::encode_raw(b"response", None).unwrap();
            stream.send_headers(&HeaderMap::new()).unwrap();
            stream.send_message(resp_frame).unwrap();
            stream.send_trailers(&Status::ok(), &HeaderMap::new()).unwrap();
        });

        // ── Client ────────────────────────────────────────────────────────────
        let (mut transport, conn_fut) = ClientTransport::connect(client_io).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport
            .new_stream("/test.Service/Unary", &HeaderMap::new())
            .unwrap();

        let req_frame = codec::encode_raw(b"request", None).unwrap();
        stream.send_message(req_frame, true).unwrap(); // true = END_STREAM

        let headers = stream.recv_headers().await.unwrap();
        assert_eq!(headers.get("content-type").unwrap(), "application/grpc");

        let frame = stream.recv_message().await.unwrap().unwrap();
        let payload = codec::decode_raw(&frame, None, codec::DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(payload, b"response");

        let (status, _trailers) = stream.recv_trailers().await.unwrap();
        assert_eq!(status.code, Code::Ok);

        server.await.unwrap();
    }

    /// Client receives a trailer-only error response.
    #[tokio::test]
    async fn client_receives_trailer_only_error() {
        use crate::codec;
        use tokio::io::duplex;

        let (server_io, client_io) = duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut transport = ServerTransport::new(server_io).await.unwrap();
            let mut stream = transport.accept().await.unwrap();
            let _: Option<bytes::Bytes> = stream.recv_message().await.unwrap();
            stream
                .send_trailers(
                    &Status::not_found("user not found"),
                    &HeaderMap::new(),
                )
                .unwrap();
        });

        let (mut transport, conn_fut) = ClientTransport::connect(client_io).await.unwrap();
        tokio::spawn(conn_fut);

        let mut stream = transport
            .new_stream("/test.Service/Get", &HeaderMap::new())
            .unwrap();
        stream
            .send_message(codec::encode_raw(b"x", None).unwrap(), true)
            .unwrap();

        stream.recv_headers().await.unwrap();
        let (status, _) = stream.recv_trailers().await.unwrap();
        assert_eq!(status.code, Code::NotFound);
        assert_eq!(status.message, "user not found");

        server.await.unwrap();
    }
}
