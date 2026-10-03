from pathlib import Path

path = Path("src/database/store.rs")
text = path.read_text()

old = "use super::manifest::{Manifest, ManifestEntry, ManifestError};\n"
new = "use super::manifest::{Manifest, ManifestEntry, ManifestError, SstableFormat, SstableIntegrity};\n"
if text.count(old) != 1:
    raise SystemExit(f"manifest import anchor mismatch: {text.count(old)}")
text = text.replace(old, new)

old = """        for entry in manifest.entries() {
            let path = sstable_dir.join(&entry.file_name);
            let table = IndexedSstable::open(&path)?;
"""
new = """        for entry in manifest.entries() {
            if entry.format != SstableFormat::V1 {
                return Err(WalBackedError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }
            let expected_checksum = match entry.integrity {
                SstableIntegrity::WholePayloadBlake3(checksum) => checksum,
                SstableIntegrity::FooterBlake3(_) => {
                    return Err(WalBackedError::ManifestSstableMismatch(
                        entry.file_name.clone(),
                    ));
                }
            };
            let path = sstable_dir.join(&entry.file_name);
            let table = IndexedSstable::open(&path)?;
"""
if text.count(old) != 1:
    raise SystemExit(f"manifest open anchor mismatch: {text.count(old)}")
text = text.replace(old, new)

old = "                || metadata.checksum != entry.checksum\n"
new = "                || metadata.checksum != expected_checksum\n"
if text.count(old) != 1:
    raise SystemExit(f"checksum comparison anchor mismatch: {text.count(old)}")
text = text.replace(old, new)

old = """            min_key: metadata.min_key.clone(),
            max_key: metadata.max_key.clone(),
            checksum: metadata.checksum,
"""
new = """            min_key: metadata.min_key.clone(),
            max_key: metadata.max_key.clone(),
            format: SstableFormat::V1,
            integrity: SstableIntegrity::WholePayloadBlake3(metadata.checksum),
"""
if text.count(old) != 1:
    raise SystemExit(f"manifest entry anchor mismatch: {text.count(old)}")
text = text.replace(old, new)

path.write_text(text)
