//! gRPC metadata: request headers and response trailers visible to RPC handlers.
//!
//! ## Key rules (from grpc-go's `metadata` package)
//!
//! - All metadata keys are lowercased.
//! - Keys ending in `"-bin"` carry binary values; they are transmitted as
//!   standard-padded base64 on the wire.
//! - Keys **not** ending in `"-bin"` carry printable ASCII values.
//! - `Metadata::from_request_headers` strips gRPC-internal and HTTP/2
//!   transport headers so handlers only see user-defined metadata.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use http::HeaderMap;

use crate::status::Status;

// ── Metadata ─────────────────────────────────────────────────────────────────

/// gRPC metadata: a multi-map from lowercase string keys to string values.
///
/// Binary metadata keys must end in `"-bin"`; their values are automatically
/// base64-encoded when converting to HTTP/2 headers and decoded when reading.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    /// Ordered list of (key, value) pairs, preserving insertion order.
    /// Keys are always lowercase.
    entries: Vec<(String, String)>,
}

impl Metadata {
    /// Create an empty Metadata map.
    pub fn new() -> Self {
        Metadata::default()
    }

    /// Build from a flat list of `("key", "value")` pairs (pairs format).
    /// Keys are lowercased automatically.
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let mut md = Metadata::new();
        for (k, v) in pairs {
            md.append(*k, *v);
        }
        md
    }

    /// Insert a value for `key`, replacing any existing values for that key.
    /// For binary keys (`-bin` suffix), `value` is the raw string; the base64
    /// encoding happens when converting to headers.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into().to_lowercase();
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push((key, value.into()));
    }

    /// Append a value for `key` (allows multiple values per key).
    pub fn append(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.push((key.into().to_lowercase(), value.into()));
    }

    /// Insert a binary value for `key` (must end in `"-bin"`).
    ///
    /// `key` must end in `"-bin"`. The value is stored as a raw string
    /// (base64 encoding is applied when converting to wire headers).
    pub fn insert_bin(&mut self, key: impl Into<String>, value: &[u8]) -> Result<(), Status> {
        let key = key.into().to_lowercase();
        if !key.ends_with("-bin") {
            return Err(Status::invalid_argument(
                "binary metadata key must end with \"-bin\"",
            ));
        }
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push((key, B64.encode(value)));
        Ok(())
    }

    /// Get the first value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<&str> {
        let key_lower = key.to_lowercase();
        self.entries
            .iter()
            .find(|(k, _)| k == &key_lower)
            .map(|(_, v)| v.as_str())
    }

    /// Get all values for `key`.
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        let key_lower = key.to_lowercase();
        self.entries
            .iter()
            .filter(|(k, _)| k == &key_lower)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Get a binary value for `key` (must end in `"-bin"`).
    /// Returns `None` if the key is absent or if base64 decoding fails.
    pub fn get_bin(&self, key: &str) -> Option<Vec<u8>> {
        let raw = self.get(key)?;
        B64.decode(raw).ok()
    }

    /// Iterate over all `(key, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ── Conversions ──────────────────────────────────────────────────────────

    /// Build `Metadata` from incoming HTTP/2 request headers, stripping all
    /// gRPC-internal and HTTP/2 transport headers.
    ///
    /// Headers removed (matches grpc-go's incoming metadata extraction):
    /// - Pseudo-headers (`:method`, `:path`, `:scheme`, `:authority`, `:status`)
    /// - `content-type`, `host`, `te`, `user-agent`, `connection`,
    ///   `transfer-encoding`, `upgrade`
    /// - All `grpc-*` headers (these are transport-level, not user metadata)
    pub fn from_request_headers(headers: &HeaderMap) -> Self {
        let mut md = Metadata::new();
        for (name, value) in headers.iter() {
            let key = name.as_str();
            if is_user_metadata_key(key) {
                if let Ok(v) = value.to_str() {
                    // For binary keys, the value on the wire is already base64;
                    // store it as-is (get_bin will decode on read).
                    md.entries.push((key.to_owned(), v.to_owned()));
                }
            }
        }
        md
    }

    /// Build `Metadata` from incoming HTTP/2 trailer headers.
    /// Strips `grpc-status` and `grpc-message` (those become `Status`).
    pub fn from_trailer_headers(headers: &HeaderMap) -> Self {
        let mut md = Metadata::new();
        for (name, value) in headers.iter() {
            let key = name.as_str();
            if key != "grpc-status" && key != "grpc-message" && key != "grpc-status-details-bin" {
                if is_user_metadata_key(key) {
                    if let Ok(v) = value.to_str() {
                        md.entries.push((key.to_owned(), v.to_owned()));
                    }
                }
            }
        }
        md
    }

    /// Convert to an `http::HeaderMap` for sending as request or response headers.
    ///
    /// Binary keys are transmitted as-is (the caller stored them base64-encoded
    /// when using `insert_bin`).
    pub fn to_header_map(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in &self.entries {
            if let (Ok(name), Ok(value)) = (
                k.parse::<http::header::HeaderName>(),
                v.parse::<http::header::HeaderValue>(),
            ) {
                map.append(name, value);
            }
        }
        map
    }
}

// ── Header filtering ──────────────────────────────────────────────────────────

/// Return `true` if `key` is a user-defined metadata key (not a transport header).
fn is_user_metadata_key(key: &str) -> bool {
    // Pseudo-headers
    if key.starts_with(':') {
        return false;
    }
    // gRPC protocol headers (all grpc-* headers are transport-level)
    if key.starts_with("grpc-") {
        return false;
    }
    // HTTP/2 / HTTP/1.1 transport headers
    matches!(
        key,
        "content-type"
            | "host"
            | "te"
            | "user-agent"
            | "connection"
            | "transfer-encoding"
            | "upgrade"
    ) == false
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get_ascii() {
        let mut md = Metadata::new();
        md.insert("x-request-id", "abc123");
        assert_eq!(md.get("x-request-id"), Some("abc123"));
    }

    #[test]
    fn key_is_lowercased() {
        let mut md = Metadata::new();
        md.insert("X-Custom-Header", "value");
        assert_eq!(md.get("x-custom-header"), Some("value"));
        assert_eq!(md.get("X-Custom-Header"), Some("value")); // lookup also lowercases
    }

    #[test]
    fn insert_replaces_existing() {
        let mut md = Metadata::new();
        md.insert("key", "first");
        md.insert("key", "second");
        assert_eq!(md.get("key"), Some("second"));
        assert_eq!(md.get_all("key").len(), 1);
    }

    #[test]
    fn append_allows_multiple_values() {
        let mut md = Metadata::new();
        md.append("x-tag", "a");
        md.append("x-tag", "b");
        assert_eq!(md.get_all("x-tag"), vec!["a", "b"]);
    }

    #[test]
    fn binary_metadata_round_trip() {
        let mut md = Metadata::new();
        let bytes = b"\x00\x01\x02\x03";
        md.insert_bin("x-data-bin", bytes).unwrap();
        assert_eq!(md.get_bin("x-data-bin").unwrap(), bytes);
    }

    #[test]
    fn binary_key_without_bin_suffix_returns_error() {
        let mut md = Metadata::new();
        let result = md.insert_bin("x-data", b"oops");
        assert!(result.is_err());
    }

    #[test]
    fn from_pairs_constructor() {
        let md = Metadata::from_pairs(&[("x-a", "1"), ("x-b", "2")]);
        assert_eq!(md.get("x-a"), Some("1"));
        assert_eq!(md.get("x-b"), Some("2"));
    }

    #[test]
    fn to_header_map_round_trip() {
        let mut md = Metadata::new();
        md.insert("x-foo", "bar");
        md.insert("x-baz", "qux");
        let hm = md.to_header_map();
        assert_eq!(hm.get("x-foo").unwrap(), "bar");
        assert_eq!(hm.get("x-baz").unwrap(), "qux");
    }

    #[test]
    fn from_request_headers_filters_transport_headers() {
        let mut hm = HeaderMap::new();
        hm.insert("content-type", "application/grpc".parse().unwrap());
        hm.insert("te", "trailers".parse().unwrap());
        hm.insert("grpc-encoding", "gzip".parse().unwrap());
        hm.insert("user-agent", "grpc-rs/0.1".parse().unwrap());
        hm.insert("x-custom", "my-value".parse().unwrap());

        let md = Metadata::from_request_headers(&hm);
        // Transport headers stripped
        assert!(md.get("content-type").is_none());
        assert!(md.get("te").is_none());
        assert!(md.get("grpc-encoding").is_none());
        assert!(md.get("user-agent").is_none());
        // User metadata preserved
        assert_eq!(md.get("x-custom"), Some("my-value"));
    }

    #[test]
    fn from_trailer_headers_filters_grpc_status() {
        let mut hm = HeaderMap::new();
        hm.insert("grpc-status", "0".parse().unwrap());
        hm.insert("grpc-message", "ok".parse().unwrap());
        hm.insert("x-server-id", "node-1".parse().unwrap());

        let md = Metadata::from_trailer_headers(&hm);
        assert!(md.get("grpc-status").is_none());
        assert!(md.get("grpc-message").is_none());
        assert_eq!(md.get("x-server-id"), Some("node-1"));
    }
}
