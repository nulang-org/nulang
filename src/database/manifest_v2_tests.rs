use super::manifest::{
    Manifest, ManifestEntry, ManifestError, SstableFormat, SstableIntegrity,
};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_manifest_v2_{name}_{}_{}.manifest",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn entry(name: &str, format: SstableFormat, integrity: SstableIntegrity) -> ManifestEntry {
    ManifestEntry {
        file_name: name.to_owned(),
        tablet_id: 42,
        ownership_epoch: 7,
        min_sequence: 1,
        max_sequence: 3,
        row_count: 2,
        min_key: b"a".to_vec(),
        max_key: b"z".to_vec(),
        format,
        integrity,
    }
}

#[test]
fn new_manifest_publications_encode_v2_format_and_integrity_contract() {
    let path = temp_path("publish");
    let _ = fs::remove_file(&path);

    let mut manifest = Manifest::empty(42);
    manifest
        .register(entry(
            "0001.sst",
            SstableFormat::V1,
            SstableIntegrity::WholePayloadBlake3([1; 32]),
        ))
        .unwrap();
    manifest
        .register(entry(
            "0002.sst",
            SstableFormat::V2,
            SstableIntegrity::FooterBlake3([2; 32]),
        ))
        .unwrap();
    manifest.publish(&path).unwrap();

    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"NUDBMAN2");

    let reopened = Manifest::load_or_empty(&path, 42).unwrap();
    assert_eq!(reopened.entries().len(), 2);
    assert_eq!(reopened.entries()[0].format, SstableFormat::V1);
    assert_eq!(
        reopened.entries()[0].integrity,
        SstableIntegrity::WholePayloadBlake3([1; 32])
    );
    assert_eq!(reopened.entries()[1].format, SstableFormat::V2);
    assert_eq!(
        reopened.entries()[1].integrity,
        SstableIntegrity::FooterBlake3([2; 32])
    );

    let _ = fs::remove_file(path);
}

#[test]
fn legacy_v1_manifest_decodes_as_explicit_v1_whole_payload_integrity() {
    let path = temp_path("legacy");
    let _ = fs::remove_file(&path);

    let payload = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "tablet_id": 42,
        "entries": [{
            "file_name": "legacy.sst",
            "tablet_id": 42,
            "ownership_epoch": 7,
            "min_sequence": 1,
            "max_sequence": 3,
            "row_count": 2,
            "min_key": [97],
            "max_key": [122],
            "checksum": vec![9_u8; 32],
        }]
    }))
    .unwrap();
    let mut file = File::create(&path).unwrap();
    file.write_all(b"NUDBMAN1").unwrap();
    file.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    file.write_all(&payload).unwrap();
    file.write_all(blake3::hash(&payload).as_bytes()).unwrap();
    file.sync_data().unwrap();
    drop(file);

    let manifest = Manifest::load_or_empty(&path, 42).unwrap();
    assert_eq!(manifest.entries().len(), 1);
    assert_eq!(manifest.entries()[0].format, SstableFormat::V1);
    assert_eq!(
        manifest.entries()[0].integrity,
        SstableIntegrity::WholePayloadBlake3([9; 32])
    );

    let _ = fs::remove_file(path);
}

#[test]
fn format_and_integrity_kind_must_match() {
    let mut manifest = Manifest::empty(42);
    assert_eq!(
        manifest
            .register(entry(
                "bad-v1.sst",
                SstableFormat::V1,
                SstableIntegrity::FooterBlake3([1; 32]),
            ))
            .unwrap_err(),
        ManifestError::IntegrityKindMismatch("bad-v1.sst".to_owned())
    );
    assert_eq!(
        manifest
            .register(entry(
                "bad-v2.sst",
                SstableFormat::V2,
                SstableIntegrity::WholePayloadBlake3([2; 32]),
            ))
            .unwrap_err(),
        ManifestError::IntegrityKindMismatch("bad-v2.sst".to_owned())
    );
}
