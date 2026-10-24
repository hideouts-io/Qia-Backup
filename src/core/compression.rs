//! Blob compression. New backup data uses LZ4 blocks prefixed with a 4-byte
//! big-endian uncompressed length.

use crate::core::error::{BackupError, Result};
use crate::core::model::{COMPRESSION_GZIP, COMPRESSION_LZ4, COMPRESSION_NONE};

pub struct Compressed {
    pub bytes: Vec<u8>,
    pub compression_type: u32,
}

/// Compress a blob for storage. Small or incompressible blobs are stored
/// verbatim, matching the `compressionType == 0` no-compression fallback.
pub fn compress_blob(data: &[u8]) -> Compressed {
    if data.len() < 128 {
        return Compressed {
            bytes: data.to_vec(),
            compression_type: COMPRESSION_NONE,
        };
    }
    let compressed = lz4_flex::block::compress(data);
    if compressed.len() + 4 >= data.len() {
        return Compressed {
            bytes: data.to_vec(),
            compression_type: COMPRESSION_NONE,
        };
    }
    let mut out = Vec::with_capacity(4 + compressed.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&compressed);
    Compressed {
        bytes: out,
        compression_type: COMPRESSION_LZ4,
    }
}

pub fn decompress_blob(data: &[u8], compression_type: u32) -> Result<Vec<u8>> {
    match compression_type {
        COMPRESSION_NONE => Ok(data.to_vec()),
        COMPRESSION_LZ4 => lz4_decompress(data),
        COMPRESSION_GZIP => {
            use std::io::Read;
            let mut decoder = flate2::read::GzDecoder::new(data);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|e| BackupError::Codec(format!("gzip decompression failed: {e}")))?;
            Ok(out)
        }
        other => Err(BackupError::Codec(format!(
            "unsupported compression type {other}"
        ))),
    }
}

pub fn lz4_compress_with_prefix(data: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::block::compress(data);
    let mut out = Vec::with_capacity(4 + compressed.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&compressed);
    out
}

pub fn lz4_decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 4 {
        return Err(BackupError::Codec("LZ4 data has no length prefix".into()));
    }
    let expected = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
    if expected > 1 << 34 {
        return Err(BackupError::Codec(format!(
            "implausible LZ4 output size {expected}"
        )));
    }
    lz4_flex::block::decompress(&data[4..], expected)
        .map_err(|e| BackupError::Codec(format!("LZ4 decompression failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_compressible() {
        let data = b"hello qia ".repeat(1000);
        let c = compress_blob(&data);
        assert_eq!(c.compression_type, COMPRESSION_LZ4);
        assert_eq!(decompress_blob(&c.bytes, c.compression_type).unwrap(), data);
    }

    #[test]
    fn roundtrip_incompressible() {
        let mut data = vec![0u8; 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2654435761) & 0xff) as u8;
        }
        let c = compress_blob(&data);
        assert_eq!(decompress_blob(&c.bytes, c.compression_type).unwrap(), data);
    }

    #[test]
    fn gzip_compatibility() {
        use std::io::Write;
        let data = b"legacy gzip payload".repeat(100);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&data).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(
            decompress_blob(&compressed, COMPRESSION_GZIP).unwrap(),
            data
        );
    }

    #[test]
    fn empty() {
        let c = compress_blob(&[]);
        assert_eq!(decompress_blob(&c.bytes, c.compression_type).unwrap(), b"");
    }
}
