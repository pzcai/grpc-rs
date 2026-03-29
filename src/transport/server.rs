//! Server-side HTTP/2 transport.
//!
//! ## Architecture
//!
//! The h2 `Connection` must be polled continuously to drive both inbound DATA frame
//! delivery to streams AND outbound frame flushing from streams.  If we returned the
//! Connection to the caller (like the client transport does), the caller would have to
//! poll it while simultaneously using the stream — creating a two-future select that
//! every caller would have to write.
//!
//! Instead, `ServerTransport::new` spawns an accept-loop task that owns the `Connection`
//! and continuously drives it.  Accepted `ServerStream`s are delivered via an mpsc
//! channel.  This pattern mirrors grpc-go's design where a dedicated goroutine runs the
//! HTTP/2 reader loop and dispatches streams to handler goroutines.

use bytes::{Bytes, BytesMut};
use h2::server::SendResponse;
use h2::RecvStream;
use http::{HeaderMap, Request, Response};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::status::Status;
use super::try_extract_frame;

// ── ServerTransport ───────────────────────────────────────────────────────────

/// Server-side HTTP/2 transport for a single connection.
///
/// After construction the connection is driven by an internal tokio task.
/// Call [`accept`] to receive incoming RPC streams.
pub struct ServerTransport {
    rx: mpsc::Receiver<ServerStream>,
}

impl ServerTransport {
    /// Perform the HTTP/2 server handshake on `io` and start the accept loop.
    ///
    /// `T` must be `Send + 'static` because the connection loop is spawned as
    /// a separate tokio task.
    pub async fn new<T>(io: T) -> Result<Self, Status>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let conn = h2::server::Builder::new()
            .initial_window_size(4 * 1024 * 1024)
            .initial_connection_window_size(4 * 1024 * 1024)
            .handshake(io)
            .await
            .map_err(|e| Status::internal(format!("HTTP/2 server handshake: {e}")))?;

        // Channel capacity: allow a small backlog of pending streams.
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(accept_loop(conn, tx));
        Ok(ServerTransport { rx })
    }

    /// Accept the next incoming RPC stream.
    ///
    /// Returns `None` when the connection has been closed (GOAWAY or EOF) and all
    /// pending streams have been delivered.
    pub async fn accept(&mut self) -> Option<ServerStream> {
        self.rx.recv().await
    }
}

/// Continuously drives the h2 `Connection` and sends accepted streams to `tx`.
///
/// This task runs until the connection closes (peer sent GOAWAY, TCP closed) or
/// the `ServerTransport` receiver is dropped.
async fn accept_loop<T: AsyncRead + AsyncWrite + Unpin>(
    mut conn: h2::server::Connection<T, Bytes>,
    tx: mpsc::Sender<ServerStream>,
) {
    while let Some(result) = conn.accept().await {
        match result {
            Ok((request, send_response)) => {
                let stream = ServerStream::new(request, send_response);
                if tx.send(stream).await.is_err() {
                    // ServerTransport was dropped; stop accepting.
                    break;
                }
            }
            Err(_e) => {
                // Protocol error on the connection; log and stop.
                // TODO: surface connection errors once we have a logging facade.
                break;
            }
        }
    }
}

// ── ServerStream ──────────────────────────────────────────────────────────────

/// Send-side state for a single server RPC stream.
enum SendState {
    AwaitingHeaders(SendResponse<Bytes>),
    Streaming(h2::SendStream<Bytes>),
    Done,
}

/// A single gRPC RPC stream on the server side.
pub struct ServerStream {
    method: String,
    request_headers: HeaderMap,
    recv: RecvStream,
    send: SendState,
    buf: BytesMut,
    recv_done: bool,
}

impl ServerStream {
    fn new(request: Request<RecvStream>, send_response: SendResponse<Bytes>) -> Self {
        let (parts, body) = request.into_parts();
        let method = parts.uri.path().to_owned();
        ServerStream {
            method,
            request_headers: parts.headers,
            recv: body,
            send: SendState::AwaitingHeaders(send_response),
            buf: BytesMut::new(),
            recv_done: false,
        }
    }

    /// The RPC method path, e.g. `/helloworld.Greeter/SayHello`.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Request headers sent by the client (not including HTTP/2 pseudo-headers).
    pub fn request_headers(&self) -> &HeaderMap {
        &self.request_headers
    }

    /// Read the next gRPC message from the client.
    ///
    /// Returns the raw frame bytes (5-byte header + payload). Pass these to
    /// [`crate::codec::decode_raw`] or [`crate::codec::decode_message`].
    ///
    /// Returns `Ok(None)` when the client has sent END_STREAM and all messages
    /// have been consumed.
    pub async fn recv_message(&mut self) -> Result<Option<Bytes>, Status> {
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

            match self.recv.data().await {
                Some(Ok(data)) => {
                    let n = data.len();
                    self.buf.extend_from_slice(&data);
                    self.recv
                        .flow_control()
                        .release_capacity(n)
                        .map_err(|e| Status::internal(format!("release_capacity: {e}")))?;
                }
                Some(Err(e)) => {
                    return Err(Status::internal(format!("recv data frame: {e}")));
                }
                None => {
                    self.recv_done = true;
                }
            }
        }
    }

    /// Send initial response headers.
    ///
    /// Sends `:status: 200`, `content-type: application/grpc`, plus any entries
    /// in `extra_headers`. Must be called exactly once, before `send_message`.
    pub fn send_headers(&mut self, extra_headers: &HeaderMap) -> Result<(), Status> {
        let mut builder = Response::builder()
            .status(200)
            .header("content-type", "application/grpc");
        for (name, value) in extra_headers.iter() {
            builder = builder.header(name, value);
        }
        let response = builder
            .body(())
            .map_err(|e| Status::internal(format!("build response: {e}")))?;

        let prev = std::mem::replace(&mut self.send, SendState::Done);
        match prev {
            SendState::AwaitingHeaders(mut responder) => {
                let send_stream = responder
                    .send_response(response, false)
                    .map_err(|e| Status::internal(format!("send_response: {e}")))?;
                self.send = SendState::Streaming(send_stream);
                Ok(())
            }
            other => {
                self.send = other;
                Err(Status::internal("send_headers called after headers already sent"))
            }
        }
    }

    /// Send a gRPC data frame to the client.
    ///
    /// `data` must be already-framed bytes from `codec::encode_raw` or
    /// `codec::encode_message`. `send_headers` must have been called first.
    pub fn send_message(&mut self, data: Bytes) -> Result<(), Status> {
        match &mut self.send {
            SendState::Streaming(stream) => stream
                .send_data(data, false)
                .map_err(|e| Status::internal(format!("send_data: {e}"))),
            SendState::AwaitingHeaders(_) => {
                Err(Status::internal("send_message called before send_headers"))
            }
            SendState::Done => Err(Status::internal("send_message on closed stream")),
        }
    }

    /// Send trailing HEADERS with `grpc-status` / `grpc-message` and END_STREAM.
    ///
    /// For trailer-only responses (error before any data), `send_headers` need not
    /// have been called; it is invoked automatically here.
    pub fn send_trailers(&mut self, status: &Status, extra: &HeaderMap) -> Result<(), Status> {
        if matches!(self.send, SendState::AwaitingHeaders(_)) {
            self.send_headers(&HeaderMap::new())?;
        }

        let mut trailers = HeaderMap::new();
        let code_str = (status.code as u32).to_string();
        trailers.insert(
            "grpc-status",
            code_str
                .parse()
                .map_err(|e| Status::internal(format!("grpc-status header: {e}")))?,
        );
        if !status.message.is_empty() {
            trailers.insert(
                "grpc-message",
                percent_encode_grpc_message(&status.message)
                    .parse()
                    .map_err(|e| Status::internal(format!("grpc-message header: {e}")))?,
            );
        }
        for (name, value) in extra.iter() {
            trailers.append(name, value.clone());
        }

        let prev = std::mem::replace(&mut self.send, SendState::Done);
        match prev {
            SendState::Streaming(mut stream) => stream
                .send_trailers(trailers)
                .map_err(|e| Status::internal(format!("send_trailers: {e}"))),
            _ => Err(Status::internal("unexpected send state in send_trailers")),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Percent-encode a gRPC status message per the gRPC HTTP/2 protocol spec.
/// Only bytes outside 0x20–0x7E and the `%` character itself are encoded.
pub(crate) fn percent_encode_grpc_message(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b == b'%' || !(0x20..=0x7E).contains(&b) {
            let hi = b >> 4;
            let lo = b & 0x0F;
            out.push('%');
            out.push(char::from_digit(hi as u32, 16).unwrap().to_ascii_uppercase());
            out.push(char::from_digit(lo as u32, 16).unwrap().to_ascii_uppercase());
        } else {
            out.push(b as char);
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── percent_encode_grpc_message ──────────────────────────────────────────

    #[test]
    fn encode_plain_ascii_unchanged() {
        assert_eq!(percent_encode_grpc_message("hello world"), "hello world");
    }

    #[test]
    fn encode_percent_sign_is_escaped() {
        assert_eq!(percent_encode_grpc_message("50% off"), "50%25 off");
    }

    #[test]
    fn encode_non_printable_byte() {
        assert_eq!(percent_encode_grpc_message("\x01"), "%01");
    }

    #[test]
    fn encode_non_ascii_utf8() {
        // "é" is UTF-8 0xC3 0xA9
        assert_eq!(percent_encode_grpc_message("é"), "%C3%A9");
    }

    #[test]
    fn encode_empty_string() {
        assert_eq!(percent_encode_grpc_message(""), "");
    }

    // ── Integration: ServerTransport ────────────────────────────────────────

    /// End-to-end: raw h2 client sends one gRPC message; ServerTransport echoes it.
    #[tokio::test]
    async fn server_stream_echo_round_trip() {
        use crate::codec;
        use tokio::io::duplex;

        let (server_io, client_io) = duplex(4 * 1024 * 1024);

        let server = tokio::spawn(async move {
            let mut transport = ServerTransport::new(server_io).await.unwrap();
            let mut stream = transport.accept().await.unwrap();

            assert_eq!(stream.method(), "/test.Echo/Echo");
            assert_eq!(
                stream.request_headers().get("content-type").unwrap(),
                "application/grpc"
            );

            let frame = stream.recv_message().await.unwrap().unwrap();
            assert!(stream.recv_message().await.unwrap().is_none());

            stream.send_headers(&HeaderMap::new()).unwrap();
            stream.send_message(frame).unwrap();
            stream.send_trailers(&Status::ok(), &HeaderMap::new()).unwrap();
        });

        let (mut client, conn) = h2::client::handshake(client_io).await.unwrap();
        tokio::spawn(async move { conn.await.ok(); });

        let request = http::Request::builder()
            .method("POST")
            .uri("/test.Echo/Echo")
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(())
            .unwrap();

        let (resp_fut, mut send) = client.send_request(request, false).unwrap();
        let frame = codec::encode_raw(b"ping", None).unwrap();
        send.send_data(frame, true).unwrap();

        let response = resp_fut.await.unwrap();
        assert_eq!(response.status(), 200);

        let mut body = response.into_body();
        let data = body.data().await.unwrap().unwrap();
        body.flow_control().release_capacity(data.len()).unwrap();

        let decoded = codec::decode_raw(&data, None, codec::DEFAULT_MAX_RECV_SIZE).unwrap();
        assert_eq!(decoded, b"ping");

        let trailers = body.trailers().await.unwrap().unwrap();
        assert_eq!(trailers.get("grpc-status").unwrap(), "0");

        server.await.unwrap();
    }

    /// Trailer-only response: server sends error without any data.
    #[tokio::test]
    async fn server_stream_trailer_only_response() {
        use tokio::io::duplex;

        let (server_io, client_io) = duplex(4 * 1024 * 1024);

        let server = tokio::spawn(async move {
            let mut transport = ServerTransport::new(server_io).await.unwrap();
            let mut stream = transport.accept().await.unwrap();
            stream.recv_message().await.unwrap();
            stream
                .send_trailers(
                    &Status::unimplemented("not implemented yet"),
                    &HeaderMap::new(),
                )
                .unwrap();
        });

        let (mut client, conn) = h2::client::handshake(client_io).await.unwrap();
        tokio::spawn(async move { conn.await.ok(); });

        let request = http::Request::builder()
            .method("POST")
            .uri("/test.Echo/NotImpl")
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(())
            .unwrap();

        let (resp_fut, mut send) = client.send_request(request, false).unwrap();
        send.send_data(crate::codec::encode_raw(b"x", None).unwrap(), true)
            .unwrap();

        let response = resp_fut.await.unwrap();
        assert_eq!(response.status(), 200);

        let mut body = response.into_body();
        let trailers = body.trailers().await.unwrap().unwrap();
        assert_eq!(trailers.get("grpc-status").unwrap(), "12"); // Unimplemented
        assert!(trailers
            .get("grpc-message")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("not implemented yet"));

        server.await.unwrap();
    }

    /// Calling send_headers twice returns an error.
    #[tokio::test]
    async fn send_headers_twice_errors() {
        use tokio::io::duplex;
        use tokio::sync::oneshot;

        let (server_io, client_io) = duplex(4 * 1024 * 1024);
        let (done_tx, done_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            let (mut client, conn) = h2::client::handshake(client_io).await.unwrap();
            tokio::spawn(async move { conn.await.ok(); });
            let request = http::Request::builder()
                .method("POST")
                .uri("/x/x")
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .body(())
                .unwrap();
            let (_resp, mut send) = client.send_request(request, false).unwrap();
            send.send_data(crate::codec::encode_raw(b"x", None).unwrap(), true)
                .unwrap();
            done_rx.await.ok();
        });

        let mut transport = ServerTransport::new(server_io).await.unwrap();
        let mut stream = transport.accept().await.unwrap();
        stream.recv_message().await.unwrap();

        stream.send_headers(&HeaderMap::new()).unwrap();
        let result = stream.send_headers(&HeaderMap::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("already sent"));

        done_tx.send(()).ok();
    }
}
