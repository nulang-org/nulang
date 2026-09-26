use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveCompression {
    Gzip,
    Zstd,
}

impl ArchiveCompression {
    pub fn content_type(self) -> &'static str {
        todo!("RED: archive content type")
    }

    pub fn extension(self) -> &'static str {
        todo!("RED: archive extension")
    }
}

pub fn detect_archive_compression(_bytes: &[u8]) -> Option<ArchiveCompression> {
    todo!("RED: archive magic detection")
}

pub fn compress_tar(_tar_bytes: &[u8], _compression: ArchiveCompression) -> io::Result<Vec<u8>> {
    todo!("RED: archive compression")
}

pub fn decode_archive(_archive_bytes: &[u8]) -> io::Result<Vec<u8>> {
    todo!("RED: archive decoding")
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

    #[test]
    fn metadata_is_stable() {
        assert_eq!(ArchiveCompression::Gzip.content_type(), "application/gzip");
        assert_eq!(ArchiveCompression::Gzip.extension(), ".tar.gz");
        assert_eq!(ArchiveCompression::Zstd.content_type(), "application/zstd");
        assert_eq!(ArchiveCompression::Zstd.extension(), ".tar.zst");
    }
}
