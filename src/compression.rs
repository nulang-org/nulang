//! Versioned compression envelope for runtime blobs.
//!
//! The envelope is intentionally independent from the payload format. Runtime
//! checkpoint/hibernation formats stay stable and can be wrapped without
//! teaching their decoders about compression.
//!
//! Layout (big-endian):
//!   magic[4] = "NUZ0"
//!   version  = u8
//!   codec    = u8 (0 = raw, 1 = zstd)
//!   reserved = u16
//!   original_len = u64
//!   payload  = remaining bytes

use std::borrow::Cow;
use std::io;

pub const BLOB_MAGIC: [u8; 4] = *b"NUZ0";
pub const BLOB_VERSION: u8 = 1;
pub const BLOB_HEADER_LEN: usize = 16;
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const DEFAULT_COMPRESSION_THRESHOLD: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlobCodec {
    Raw = 0,
    Zstd = 1,
}

pub fn encode_blob(_input: &[u8]) -> io::Result<Vec<u8>> {
    todo!("RED: implement versioned blob compression envelope")
}

pub fn encode_blob_with_threshold(_input: &[u8], _threshold: usize) -> io::Result<Vec<u8>> {
    todo!("RED: implement threshold-aware blob compression envelope")
}

pub fn decode_blob(_encoded: &[u8]) -> io::Result<Vec<u8>> {
    todo!("RED: implement versioned blob decompression")
}

pub fn decode_blob_or_raw<'a>(_bytes: &'a [u8]) -> io::Result<Cow<'a, [u8]>> {
    todo!("RED: implement legacy raw compatibility")
}

pub fn envelope_codec(_bytes: &[u8]) -> Option<BlobCodec> {
    todo!("RED: implement envelope inspection")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payload_uses_raw_codec_and_round_trips() {
        let input = b"small actor continuation";
        let encoded = encode_blob_with_threshold(input, 1024).unwrap();

        assert_eq!(envelope_codec(&encoded), Some(BlobCodec::Raw));
        assert_eq!(decode_blob(&encoded).unwrap(), input);
    }

    #[test]
    fn repetitive_large_payload_uses_zstd_and_round_trips() {
        let input = vec![b'x'; 64 * 1024];
        let encoded = encode_blob_with_threshold(&input, 1024).unwrap();

        assert_eq!(envelope_codec(&encoded), Some(BlobCodec::Zstd));
        assert!(encoded.len() < input.len() / 4);
        assert_eq!(decode_blob(&encoded).unwrap(), input);
    }

    #[test]
    fn legacy_unwrapped_payload_is_borrowed_without_copy() {
        let legacy = b"NHS0 legacy continuation bytes";
        let decoded = decode_blob_or_raw(legacy).unwrap();

        assert!(matches!(decoded, Cow::Borrowed(_)));
        assert_eq!(decoded.as_ref(), legacy);
    }

    #[test]
    fn truncated_envelope_fails_closed() {
        let truncated = [BLOB_MAGIC.as_slice(), &[BLOB_VERSION, BlobCodec::Raw as u8]].concat();
        let error = decode_blob(&truncated).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
