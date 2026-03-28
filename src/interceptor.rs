//! gRPC interceptor (middleware) API — client and server side.
//!
//! ## Design
//!
//! Interceptors sit between the gRPC framework and the application handler (server-side)
//! or between the application caller and the transport (client-side).  They receive a
//! `next` invocable so they can modify the request/response, add metadata, log, etc.
//!
//! Both client and server interceptors operate at the **raw-bytes** level: they see the
//! serialized protobuf payload (no gRPC framing), matching grpc-go's
//! `UnaryServerInterceptor` / `UnaryClientInterceptor` signatures.
//!
//! ## Chain building
//!
//! Multiple interceptors are composed with [`chain_server`] / [`chain_client`].  The
//! first interceptor in the slice is the outermost (first to run on the way in, last on
//! the way out).

use std::sync::Arc;

use bytes::Bytes;

use crate::metadata::Metadata;
use crate::server::BoxFuture;
use crate::status::Status;

// ── Type aliases ─────────────────────────────────────────────────────────────

/// The "next" invoker passed to a server interceptor.
///
/// Calling it invokes the next interceptor in the chain, or the handler itself.
pub type ServerNext =
    Arc<dyn Fn(Bytes, Metadata) -> BoxFuture<Result<Bytes, Status>> + Send + Sync + 'static>;

/// A server-side unary interceptor.
///
/// Arguments:
/// - `method`: the full method path (e.g. `"/helloworld.Greeter/SayHello"`).
/// - `req`: the decoded request payload (raw protobuf bytes, no gRPC framing).
/// - `metadata`: request metadata.
/// - `next`: call this to pass control to the next interceptor / handler.
pub type UnaryServerInterceptor = Arc<
    dyn Fn(String, Bytes, Metadata, ServerNext) -> BoxFuture<Result<Bytes, Status>>
        + Send
        + Sync
        + 'static,
>;

/// The "next" invoker passed to a client interceptor.
pub type ClientNext = Arc<
    dyn Fn(Bytes, Metadata) -> BoxFuture<Result<(Bytes, Metadata), Status>> + Send + Sync + 'static,
>;

/// A client-side unary interceptor.
///
/// Arguments:
/// - `method`: the full method path.
/// - `req`: the encoded request payload (raw protobuf bytes, no gRPC framing).
/// - `metadata`: request metadata (may be cloned/modified before passing to `next`).
/// - `next`: call this to pass control to the next interceptor / transport layer.
pub type UnaryClientInterceptor = Arc<
    dyn Fn(String, Bytes, Metadata, ClientNext)
            -> BoxFuture<Result<(Bytes, Metadata), Status>>
        + Send
        + Sync
        + 'static,
>;

// ── Chain building ────────────────────────────────────────────────────────────

/// Build a `ServerNext` chain from a slice of interceptors and a terminal handler.
///
/// `interceptors[0]` is the outermost (runs first), `handler` is the innermost.
pub fn chain_server(
    interceptors: &[UnaryServerInterceptor],
    method: String,
    handler: ServerNext,
) -> ServerNext {
    let mut next = handler;
    // Build from inside out: wrap handler with last interceptor first.
    for interceptor in interceptors.iter().rev() {
        let interceptor = Arc::clone(interceptor);
        let inner = Arc::clone(&next);
        let m = method.clone();
        next = Arc::new(move |req: Bytes, md: Metadata| {
            let interceptor = Arc::clone(&interceptor);
            let inner = Arc::clone(&inner);
            let method = m.clone();
            interceptor(method, req, md, inner)
        });
    }
    next
}

/// Build a `ClientNext` chain from a slice of interceptors and a terminal invoker.
///
/// `interceptors[0]` is the outermost (runs first), `invoker` is the innermost.
pub fn chain_client(
    interceptors: &[UnaryClientInterceptor],
    method: String,
    invoker: ClientNext,
) -> ClientNext {
    let mut next = invoker;
    for interceptor in interceptors.iter().rev() {
        let interceptor = Arc::clone(interceptor);
        let inner = Arc::clone(&next);
        let m = method.clone();
        next = Arc::new(move |req: Bytes, md: Metadata| {
            let interceptor = Arc::clone(&interceptor);
            let inner = Arc::clone(&inner);
            let method = m.clone();
            interceptor(method, req, md, inner)
        });
    }
    next
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── Server interceptor chain ──────────────────────────────────────────────

    /// An interceptor that prepends "A:" to the request bytes and appends ":A" to the response.
    fn tag_interceptor(tag: &'static str) -> UnaryServerInterceptor {
        Arc::new(move |_method: String, req, md, next| {
            Box::pin(async move {
                let mut tagged_req = format!("{tag}:").into_bytes();
                tagged_req.extend_from_slice(&req);
                let resp = next(Bytes::from(tagged_req), md).await?;
                let mut out = resp.to_vec();
                out.extend_from_slice(format!(":{tag}").as_bytes());
                Ok(Bytes::from(out))
            })
        })
    }

    #[tokio::test]
    async fn server_interceptor_chain_wraps_in_order() {
        let interceptors = vec![tag_interceptor("outer"), tag_interceptor("inner")];

        // Terminal handler: echo the request back.
        let handler: ServerNext = Arc::new(|req, _md| Box::pin(async move { Ok(req) }));

        let chain = chain_server(&interceptors, "/test/echo".to_owned(), handler);

        let result = chain(Bytes::from("data"), Metadata::new()).await.unwrap();
        // outer wraps inner wraps handler:
        // req goes: data → outer:data → inner:outer:data (handler echoes it)
        // resp comes back: inner:outer:data → inner:outer:data:inner → inner:outer:data:inner:outer
        assert_eq!(result, b"inner:outer:data:inner:outer".as_ref());
    }

    #[tokio::test]
    async fn server_interceptor_can_short_circuit() {
        let interceptors = vec![Arc::new(
            |_method: String, _req: Bytes, _md: Metadata, _next: ServerNext| {
                Box::pin(async move {
                    Err(Status::unauthenticated("not allowed"))
                }) as BoxFuture<_>
            },
        ) as UnaryServerInterceptor];

        let handler: ServerNext = Arc::new(|_req, _md| {
            Box::pin(async move { panic!("handler must not be called") })
        });

        let chain = chain_server(&interceptors, "/test/nope".to_owned(), handler);
        let err = chain(Bytes::new(), Metadata::new()).await.unwrap_err();
        assert_eq!(err.code, crate::status::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn server_interceptor_can_modify_metadata() {
        use std::sync::Mutex;
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured);

        let interceptors = vec![Arc::new(
            move |_method: String, req: Bytes, mut md: Metadata, next: ServerNext| {
                let captured_clone = Arc::clone(&captured_clone);
                Box::pin(async move {
                    md.insert("x-intercepted", "yes");
                    let resp = next(req, md.clone()).await?;
                    *captured_clone.lock().unwrap() = md.get("x-intercepted").map(String::from);
                    Ok(resp)
                }) as BoxFuture<_>
            },
        ) as UnaryServerInterceptor];

        let handler: ServerNext = Arc::new(|req, _md| Box::pin(async move { Ok(req) }));
        let chain = chain_server(&interceptors, "/test/md".to_owned(), handler);
        chain(Bytes::from("payload"), Metadata::new()).await.unwrap();

        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some("yes")
        );
    }

    // ── Client interceptor chain ──────────────────────────────────────────────

    #[tokio::test]
    async fn client_interceptor_chain_wraps_in_order() {
        fn tag_client_interceptor(tag: &'static str) -> UnaryClientInterceptor {
            Arc::new(move |_method: String, req, md, next| {
                Box::pin(async move {
                    let mut tagged = format!("{tag}:").into_bytes();
                    tagged.extend_from_slice(&req);
                    let (resp_bytes, trailing_md) = next(Bytes::from(tagged), md).await?;
                    let mut out = resp_bytes.to_vec();
                    out.extend_from_slice(format!(":{tag}").as_bytes());
                    Ok((Bytes::from(out), trailing_md))
                })
            })
        }

        let interceptors = vec![tag_client_interceptor("A"), tag_client_interceptor("B")];

        let invoker: ClientNext = Arc::new(|req: Bytes, _md| {
            Box::pin(async move { Ok((req, Metadata::new())) })
        });

        let chain = chain_client(&interceptors, "/test/call".to_owned(), invoker);
        let (resp, _) = chain(Bytes::from("x"), Metadata::new()).await.unwrap();
        // req: x → A:x → B:A:x (invoker echoes); resp: B:A:x → B:A:x:B → B:A:x:B:A
        assert_eq!(resp, b"B:A:x:B:A".as_ref());
    }

    #[tokio::test]
    async fn empty_interceptor_chain_calls_handler_directly() {
        let handler: ServerNext = Arc::new(|_req, _md| {
            Box::pin(async move { Ok(Bytes::from("direct")) })
        });
        let chain = chain_server(&[], "/test/direct".to_owned(), handler);
        let result = chain(Bytes::new(), Metadata::new()).await.unwrap();
        assert_eq!(result, b"direct".as_ref());
    }
}
