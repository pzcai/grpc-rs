use std::io::{Read, Write};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};

use crate::status::Status;

use super::Compressor;

/// gzip compressor backed by the `flate2` crate.
///
/// Thread-safe: each call to `compress`/`decompress` creates a fresh encoder/decoder.
/// Unlike grpc-go's pool-based implementation, we rely on the allocator's efficiency
/// rather than maintaining a sync.Pool. This can be revisited if profiling shows
/// allocation pressure at high throughput.
pub struct GzipCompressor {
    level: Compression,
}

impl GzipCompressor {
    pub fn new() -> Self {
        GzipCompressor { level: Compression::default() }
    }

    pub fn with_level(level: u32) -> Self {
        GzipCompressor { level: Compression::new(level) }
    }
}

impl Default for GzipCompressor {
    fn default() -> Self {
        GzipCompressor::new()
    }
}

impl Compressor for GzipCompressor {
    fn name(&self) -> &'static str {
        "gzip"
    }

    fn compress(&self, input: &[u8]) -> Result<Vec<u8>, Status> {
        let mut encoder = GzEncoder::new(Vec::new(), self.level);
        encoder
            .write_all(input)
            .map_err(|e| Status::internal(format!("gzip compress write: {e}")))?;
        encoder
            .finish()
            .map_err(|e| Status::internal(format!("gzip compress finish: {e}")))
    }

    fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, Status> {
        let mut decoder = GzDecoder::new(input);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| Status::internal(format!("gzip decompress: {e}")))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_nonempty() {
        let c = GzipCompressor::new();
        let data = b"hello gRPC world, this is a test payload";
        let compressed = c.compress(data).unwrap();
        let decompressed = c.decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn round_trip_empty() {
        let c = GzipCompressor::new();
        let compressed = c.compress(b"").unwrap();
        let decompressed = c.decompress(&compressed).unwrap();
        assert_eq!(decompressed, b"");
    }

    #[test]
    fn decompress_invalid_returns_internal() {
        let c = GzipCompressor::new();
        let result = c.decompress(b"not valid gzip data");
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code, crate::status::Code::Internal);
    }

    #[test]
    fn name_is_gzip() {
        assert_eq!(GzipCompressor::new().name(), "gzip");
    }
}
