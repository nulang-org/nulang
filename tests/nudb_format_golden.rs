//! Immutable, independently encoded compatibility fixtures. Never rewrite them
//! from the implementation under test.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::checkpoint::CheckpointError;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::{FileWal, WalError};

const FULL: &[u8] = include_bytes!("fixtures/nudb/wal3-full.bin");
const RECLAIMED: &[u8] = include_bytes!("fixtures/nudb/wal3-reclaimed.bin");
const CANONICAL: &[u8] = include_bytes!("fixtures/nudb/ckp1-canonical.bin");
const LEGACY: &[u8] = include_bytes!("fixtures/nudb/ckp1-legacy-duplicates.bin");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new(wal: &[u8], checkpoint: Option<&[u8]>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nudb-golden-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let fixture = Self(dir);
        fs::write(fixture.wal(), wal).unwrap();
        if let Some(bytes) = checkpoint {
            fs::write(fixture.checkpoint(), bytes).unwrap();
        }
        fixture
    }
    fn wal(&self) -> PathBuf {
        self.0.join("tablet.wal")
    }
    fn checkpoint(&self) -> PathBuf {
        self.0.join("tablet.checkpoint")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(987).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap()
}
fn history(tablet: &WalBackedTablet) {
    assert_eq!(tablet.current_sequence(), 3);
    assert_eq!(tablet.read_at(b"k", 0).unwrap(), None);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&[0, 255][..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"last"[..]));
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), None);
    assert_eq!(tablet.read_at(b"m", 1).unwrap(), Some(&b""[..]));
    assert_eq!(tablet.read_at(b"m", 2).unwrap(), None);
    assert_eq!(tablet.read_at(b"n", 2).unwrap(), None);
    assert_eq!(tablet.read_at(b"n", 3).unwrap(), Some(&b"tail"[..]));
}
#[test]
fn immutable_wal_and_checkpoint_fixtures_recover_identical_history() {
    for (wal, checkpoint) in [
        (FULL, None),
        (RECLAIMED, Some(CANONICAL)),
        (RECLAIMED, Some(LEGACY)),
    ] {
        let fixture = Fixture::new(wal, checkpoint);
        let tablet = WalBackedTablet::open(descriptor(), fixture.wal()).unwrap();
        history(&tablet);
        drop(tablet);
        assert_eq!(fs::read(fixture.wal()).unwrap(), wal);
        if let Some(bytes) = checkpoint {
            assert_eq!(fs::read(fixture.checkpoint()).unwrap(), bytes);
        }
    }
}
fn put(key: u8, value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: vec![key],
        value: value.to_vec(),
    }
}
fn delete(key: u8) -> TabletMutation {
    TabletMutation::Delete { key: vec![key] }
}
fn commit(tablet: &mut WalBackedTablet, mutations: Vec<TabletMutation>) {
    let seq = tablet.current_sequence();
    let write = tablet.prepare_write(7, seq, mutations).unwrap();
    assert_eq!(tablet.commit(write).unwrap(), seq + 1);
}
#[test]
fn canonical_writer_matches_frozen_v1_encoding() {
    let fixture = Fixture::new(&[], None);
    let mut tablet = WalBackedTablet::open(descriptor(), fixture.wal()).unwrap();
    commit(&mut tablet, vec![put(b'k', &[0, 255]), put(b'm', b"")]);
    commit(
        &mut tablet,
        vec![
            put(b'k', &[1]),
            delete(b'k'),
            put(b'k', b"last"),
            delete(b'm'),
        ],
    );
    tablet.publish_checkpoint().unwrap();
    assert_eq!(fs::read(fixture.checkpoint()).unwrap(), CANONICAL);
    commit(&mut tablet, vec![delete(b'k'), put(b'n', b"tail")]);
    assert_eq!(fs::read(fixture.wal()).unwrap(), FULL);
    history(&tablet);
    drop(tablet);

    let fixture = Fixture::new(&[], None);
    let mut tablet = WalBackedTablet::open(descriptor(), fixture.wal()).unwrap();
    commit(&mut tablet, vec![put(b'k', &[0, 255]), put(b'm', b"")]);
    commit(
        &mut tablet,
        vec![
            put(b'k', &[1]),
            delete(b'k'),
            put(b'k', b"last"),
            delete(b'm'),
        ],
    );
    tablet.checkpoint().unwrap();
    commit(&mut tablet, vec![delete(b'k'), put(b'n', b"tail")]);
    assert_eq!(fs::read(fixture.wal()).unwrap(), RECLAIMED);
    assert_eq!(fs::read(fixture.checkpoint()).unwrap(), CANONICAL);
    history(&tablet);
}
fn rehash(bytes: &mut [u8], begin: usize, end: usize) {
    let hash = blake3::hash(&bytes[begin..end]);
    bytes[end..end + 32].copy_from_slice(hash.as_bytes());
}
#[test]
fn unknown_wal_versions_and_complete_corruption_are_not_repaired() {
    let mut unknown_magic = FULL.to_vec();
    unknown_magic[7] = b'9';
    rehash(&mut unknown_magic, 0, 32);
    let mut frame_version = FULL.to_vec();
    frame_version[68..70].copy_from_slice(&2u16.to_le_bytes());
    rehash(&mut frame_version, 64, 74);
    let mut record_version = FULL.to_vec();
    let payload_len = u32::from_le_bytes(record_version[70..74].try_into().unwrap()) as usize;
    // Fixed fixture begins its JSON with {"version":1.
    assert_eq!(record_version[117], b'1');
    record_version[117] = b'2';
    rehash(&mut record_version, 106, 106 + payload_len);
    let mut corrupt = FULL.to_vec();
    corrupt[106 + payload_len] ^= 1;
    for (bytes, expected) in [
        (unknown_magic, WalError::InvalidHeader),
        (
            frame_version,
            WalError::UnsupportedFrameVersion {
                offset: 64,
                version: 2,
            },
        ),
        (
            record_version,
            WalError::UnsupportedRecordVersion {
                offset: 64,
                version: 2,
            },
        ),
        (corrupt, WalError::ChecksumMismatch { offset: 64 }),
    ] {
        let fixture = Fixture::new(&bytes, None);
        assert_eq!(FileWal::open(fixture.wal()).unwrap_err(), expected);
        assert_eq!(fs::read(fixture.wal()).unwrap(), bytes);
    }
}
#[test]
fn unknown_checkpoint_version_and_checksum_corruption_fail_closed() {
    let mut version = CANONICAL.to_vec();
    let payload_len = u32::from_le_bytes(version[8..12].try_into().unwrap()) as usize;
    assert_eq!(version[23], b'1');
    version[23] = b'9';
    rehash(&mut version, 12, 12 + payload_len);
    let mut corrupt = CANONICAL.to_vec();
    corrupt[12 + payload_len] ^= 1;
    for (bytes, expected) in [
        (version, CheckpointError::UnsupportedVersion(9)),
        (corrupt, CheckpointError::ChecksumMismatch),
    ] {
        let fixture = Fixture::new(RECLAIMED, Some(&bytes));
        assert_eq!(
            WalBackedTablet::open(descriptor(), fixture.wal()).unwrap_err(),
            WalBackedError::Checkpoint(expected)
        );
        assert_eq!(fs::read(fixture.wal()).unwrap(), RECLAIMED);
        assert_eq!(fs::read(fixture.checkpoint()).unwrap(), bytes);
    }
}

#[test]
fn checksummed_foreign_checkpoint_cannot_replace_full_wal_history() {
    // Same identity, epoch and checkpoint sequence; only the historical
    // value differs. The checksum is valid and the latest value is unchanged.
    let mut foreign = CANONICAL.to_vec();
    let payload_len = u32::from_le_bytes(foreign[8..12].try_into().unwrap()) as usize;
    let start = foreign
        .windows(b"[0,255]".len())
        .position(|b| b == b"[0,255]")
        .unwrap();
    foreign[start + 1] = b'9';
    rehash(&mut foreign, 12, 12 + payload_len);
    let fixture = Fixture::new(FULL, Some(&foreign));
    assert!(
        WalBackedTablet::open(descriptor(), fixture.wal()).is_err(),
        "foreign checkpoint silently replaced acknowledged history"
    );
    assert_eq!(fs::read(fixture.wal()).unwrap(), FULL);
    assert_eq!(fs::read(fixture.checkpoint()).unwrap(), foreign);
}

#[test]
fn retained_wal_overlap_must_match_checkpoint_even_after_reclamation() {
    let fixture = Fixture::new(RECLAIMED, Some(CANONICAL));
    let tablet = WalBackedTablet::open(descriptor(), fixture.wal()).unwrap();
    tablet.publish_checkpoint().unwrap();
    drop(tablet);
    let mut foreign = fs::read(fixture.checkpoint()).unwrap();
    let payload_len = u32::from_le_bytes(foreign[8..12].try_into().unwrap()) as usize;
    // Change the value introduced at sequence 3, which is still in the WAL.
    let start = foreign
        .windows(b"[116,97,105,108]".len())
        .position(|b| b == b"[116,97,105,108]")
        .unwrap();
    foreign[start + 3] = b'7';
    rehash(&mut foreign, 12, 12 + payload_len);
    fs::write(fixture.checkpoint(), &foreign).unwrap();
    assert!(
        WalBackedTablet::open(descriptor(), fixture.wal()).is_err(),
        "foreign retained history silently accepted"
    );
    assert_eq!(fs::read(fixture.wal()).unwrap(), RECLAIMED);
    assert_eq!(fs::read(fixture.checkpoint()).unwrap(), foreign);
}
