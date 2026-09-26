use std::io::{self, Cursor, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveCompression {
    Gzip,
    Zstd,
}

impl ArchiveCompression {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Gzip => "application/gzip",
            Self::Zstd => "application/zstd",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Gzip => ".tar.gz",
            Self::Zstd => ".tar.zst",
        }
    }
}

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

pub fn detect_archive_compression(bytes: &[u8]) -> Option<ArchiveCompression> {
    if bytes.starts_with(&GZIP_MAGIC) {
        Some(ArchiveCompression::Gzip)
    } else if bytes.starts_with(&ZSTD_MAGIC) {
        Some(ArchiveCompression::Zstd)
    } else {
        None
    }
}

pub fn compress_tar(tar_bytes: &[u8], compression: ArchiveCompression) -> io::Result<Vec<u8>> {
    match compression {
        ArchiveCompression::Gzip => {
            let mut output = Vec::new();
            {
                let mut encoder =
                    flate2::write::GzEncoder::new(&mut output, flate2::Compression::default());
                encoder.write_all(tar_bytes)?;
                encoder.finish()?;
            }
            Ok(output)
        }
        ArchiveCompression::Zstd => {
            #[cfg(feature = "zstd-compression")]
            {
                zstd::stream::encode_all(Cursor::new(tar_bytes), crate::compression::DEFAULT_ZSTD_LEVEL)
            }
            #[cfg(not(feature = "zstd-compression"))]
            {
                let _ = tar_bytes;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "zstd package archives require the 'zstd-compression' feature",
                ))
            }
        }
    }
}

pub fn decode_archive(archive_bytes: &[u8]) -> io::Result<Vec<u8>> {
    match detect_archive_compression(archive_bytes) {
        Some(ArchiveCompression::Gzip) => {
            let mut decoder = flate2::read::GzDecoder::new(Cursor::new(archive_bytes));
            let mut output = Vec::new();
            decoder.read_to_end(&mut output)?;
            Ok(output)
        }
        Some(ArchiveCompression::Zstd) => {
            #[cfg(feature = "zstd-compression")]
            {
                zstd::stream::decode_all(Cursor::new(archive_bytes)).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid zstd package archive: {error}"),
                    )
                })
            }
            #[cfg(not(feature = "zstd-compression"))]
            {
                let _ = archive_bytes;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "zstd package archive requires the 'zstd-compression' feature",
                ))
            }
        }
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown package archive compression",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gzip_and_zstd_magic() {
        assert_eq!(
            detect_archive_compression(&[0x1f, 0x8b, 0x08, 0x00]),
            Some(ArchiveCompression::Gzip)
        );
        assert_eq!(
            detect_archive_compression(&[0x28, 0xb5, 0x2f, 0xfd]),
            Some(ArchiveCompression::Zstd)
        );
        assert_eq!(detect_archive_compression(b"not-an-archive"), None);
    }

    #[test]
    fn gzip_round_trip() {
        let tar = vec![b'a'; 16 * 1024];
        let archive = compress_tar(&tar, ArchiveCompression::Gzip).unwrap();
        assert_eq!(detect_archive_compression(&archive), Some(ArchiveCompression::Gzip));
        assert_eq!(decode_archive(&archive).unwrap(), tar);
    }

    #[cfg(feature = "zstd-compression")]
    #[test]
    fn zstd_round_trip_is_smaller_for_repetitive_tar_bytes() {
        let tar = vec![b'a'; 64 * 1024];
        let archive = compress_tar(&tar, ArchiveCompression::Zstd).unwrap();
        assert_eq!(detect_archive_compression(&archive), Some(ArchiveCompression::Zstd));
        assert!(archive.len() < tar.len() / 4);
        assert_eq!(decode_archive(&archive).unwrap(), tar);
    }

    #[cfg(not(feature = "zstd-compression"))]
    #[test]
    fn zstd_encode_fails_cleanly_without_feature() {
        let error = compress_tar(b"tar", ArchiveCompression::Zstd).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn unknown_archive_fails_closed() {
        let error = decode_archive(b"not-an-archive").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn metadata_is_stable() {
        assert_eq!(ArchiveCompression::Gzip.content_type(), "application/gzip");
        assert_eq!(ArchiveCompression::Gzip.extension(), ".tar.gz");
        assert_eq!(ArchiveCompression::Zstd.content_type(), "application/zstd");
        assert_eq!(ArchiveCompression::Zstd.extension(), ".tar.zst");
    }
}
