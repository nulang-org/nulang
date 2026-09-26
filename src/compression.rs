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

#[cfg(feature = "zstd-compression")]
use std::io::Cursor;

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

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn encode_envelope(codec: BlobCodec, original_len: usize, payload: &[u8]) -> io::Result<Vec<u8>> {
    let original_len = u64::try_from(original_len)
        .map_err(|_| invalid_data("runtime blob length exceeds u64"))?;
    let capacity = BLOB_HEADER_LEN
        .checked_add(payload.len())
        .ok_or_else(|| invalid_data("runtime blob envelope length overflow"))?;

    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&BLOB_MAGIC);
    encoded.push(BLOB_VERSION);
    encoded.push(codec as u8);
    encoded.extend_from_slice(&0u16.to_be_bytes());
    encoded.extend_from_slice(&original_len.to_be_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

/// Encode a runtime blob using the default compression threshold.
///
/// Small or incompressible payloads retain their bytes under the same versioned
/// envelope using BlobCodec::Raw. Large payloads use zstd only when the
/// compressed representation is actually smaller.
pub fn encode_blob(input: &[u8]) -> io::Result<Vec<u8>> {
    encode_blob_with_threshold(input, DEFAULT_COMPRESSION_THRESHOLD)
}

/// Encode a runtime blob using a caller-supplied zstd threshold.
///
/// When the zstd-compression feature is disabled, this remains fully usable
/// and emits a raw envelope. This keeps minimal builds free of the native zstd
/// dependency while preserving the envelope contract.
pub fn encode_blob_with_threshold(input: &[u8], threshold: usize) -> io::Result<Vec<u8>> {
    #[cfg(feature = "zstd-compression")]
    if input.len() >= threshold {
        let compressed = zstd::stream::encode_all(Cursor::new(input), DEFAULT_ZSTD_LEVEL)?;
        if compressed.len() < input.len() {
            return encode_envelope(BlobCodec::Zstd, input.len(), &compressed);
        }
    }

    let _ = threshold;
    encode_envelope(BlobCodec::Raw, input.len(), input)
}

/// Decode one versioned runtime blob.
///
/// Corrupt, truncated, unknown-version, unknown-codec, and length-mismatched
/// envelopes fail closed. A zstd envelope also fails with Unsupported when
/// decoded by a build that intentionally omitted zstd-compression.
pub fn decode_blob(encoded: &[u8]) -> io::Result<Vec<u8>> {
    if encoded.len() < BLOB_HEADER_LEN {
        return Err(invalid_data("truncated runtime blob envelope"));
    }
    if encoded[..4] != BLOB_MAGIC {
        return Err(invalid_data("invalid runtime blob magic"));
    }
    if encoded[4] != BLOB_VERSION {
        return Err(invalid_data(format!(
            "unsupported runtime blob version {}",
            encoded[4]
        )));
    }
    if encoded[6] != 0 || encoded[7] != 0 {
        return Err(invalid_data("runtime blob reserved bits are non-zero"));
    }

    let expected_len_u64 = u64::from_be_bytes(
        encoded[8..16]
            .try_into()
            .expect("fixed runtime blob length field"),
    );
    let expected_len = usize::try_from(expected_len_u64)
        .map_err(|_| invalid_data("runtime blob length exceeds platform usize"))?;
    let payload = &encoded[BLOB_HEADER_LEN..];

    let decoded = match encoded[5] {
        value if value == BlobCodec::Raw as u8 => {
            if payload.len() != expected_len {
                return Err(invalid_data(format!(
                    "raw runtime blob length mismatch: expected {expected_len}, got {}",
                    payload.len()
                )));
            }
            payload.to_vec()
        }
        value if value == BlobCodec::Zstd as u8 => {
            #[cfg(feature = "zstd-compression")]
            {
                zstd::stream::decode_all(Cursor::new(payload))
                    .map_err(|error| invalid_data(format!("invalid zstd runtime blob: {error}")))?
            }

            #[cfg(not(feature = "zstd-compression"))]
            {
                let _ = payload;
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "zstd runtime blob requires the zstd-compression feature",
                ));
            }
        }
        value => {
            return Err(invalid_data(format!(
                "unknown runtime blob codec {value}"
            )))
        }
    };

    if decoded.len() != expected_len {
        return Err(invalid_data(format!(
            "runtime blob length mismatch after decode: expected {expected_len}, got {}",
            decoded.len()
        )));
    }

    Ok(decoded)
}

/// Decode a versioned envelope while preserving compatibility with historical
/// unwrapped continuation/checkpoint bytes.
///
/// A payload beginning with the envelope magic is always treated as an
/// envelope. This intentionally fails closed for a truncated/corrupt envelope
/// rather than silently interpreting it as a legacy raw payload.
pub fn decode_blob_or_raw(bytes: &[u8]) -> io::Result<Cow<'_, [u8]>> {
    if bytes.starts_with(&BLOB_MAGIC) {
        decode_blob(bytes).map(Cow::Owned)
    } else {
        Ok(Cow::Borrowed(bytes))
    }
}

/// Inspect the codec of a structurally valid v1 envelope header.
pub fn envelope_codec(bytes: &[u8]) -> Option<BlobCodec> {
    if bytes.len() < BLOB_HEADER_LEN
        || bytes[..4] != BLOB_MAGIC
        || bytes[4] != BLOB_VERSION
        || bytes[6] != 0
        || bytes[7] != 0
    {
        return None;
    }

    match bytes[5] {
        value if value == BlobCodec::Raw as u8 => Some(BlobCodec::Raw),
        value if value == BlobCodec::Zstd as u8 => Some(BlobCodec::Zstd),
        _ => None,
    }
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

    #[cfg(feature = "zstd-compression")]
    #[test]
    fn repetitive_large_payload_uses_zstd_and_round_trips() {
        let input = vec![b'x'; 64 * 1024];
        let encoded = encode_blob_with_threshold(&input, 1024).unwrap();

        assert_eq!(envelope_codec(&encoded), Some(BlobCodec::Zstd));
        assert!(encoded.len() < input.len() / 4);
        assert_eq!(decode_blob(&encoded).unwrap(), input);
    }

    #[cfg(not(feature = "zstd-compression"))]
    #[test]
    fn large_payload_falls_back_to_raw_without_zstd_feature() {
        let input = vec![b'x'; 64 * 1024];
        let encoded = encode_blob_with_threshold(&input, 1024).unwrap();

        assert_eq!(envelope_codec(&encoded), Some(BlobCodec::Raw));
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

    #[test]
    fn declared_raw_length_must_match_payload() {
        let mut encoded = encode_blob_with_threshold(b"abc", usize::MAX).unwrap();
        encoded[8..16].copy_from_slice(&4u64.to_be_bytes());

        let error = decode_blob(&encoded).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_codec_fails_closed() {
        let mut encoded = encode_blob_with_threshold(b"abc", usize::MAX).unwrap();
        encoded[5] = 0xff;

        let error = decode_blob(&encoded).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
