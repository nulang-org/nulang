//! Seeded differential state-machine checks against an independent snapshot model.
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation,
};
use std::{collections::BTreeMap, fs, path::PathBuf};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    *state >> 32
}

#[test]
fn seeded_mvcc_matches_model_across_checkpoint_and_reopen() {
    let base = std::env::var("TABLET_TEST_SEED")
        .ok()
        .map(|s| s.parse::<u64>().expect("decimal seed"))
        .unwrap_or(42);
    let count = std::env::var("TABLET_TEST_COUNT")
        .ok()
        .map(|s| s.parse::<u64>().expect("decimal seed count"))
        .unwrap_or(16);
    assert!(count > 0, "seed count must be positive");
    for offset in 0..count {
        let seed = base.wrapping_add(offset);
        let mut rng = seed;
        let dir = Scratch(
            std::env::temp_dir().join(format!("nulang-tablet-model-{}-{seed}", std::process::id())),
        );
        fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join("state.wal");
        let descriptor = TabletDescriptor::new(
            TabletId::new(91).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            1,
        )
        .unwrap();
        let mut disk = WalBackedTablet::open(descriptor.clone(), &path).unwrap();
        let mut memory = MemoryTablet::new(descriptor.clone());
        let mut history = vec![BTreeMap::<Vec<u8>, Vec<u8>>::new()];
        for step in 0..48 {
            let key = vec![b'a' + (next(&mut rng) % 5) as u8];
            let mut snapshot = history.last().unwrap().clone();
            let mutation = if next(&mut rng) % 3 == 0 {
                snapshot.remove(&key);
                TabletMutation::Delete { key }
            } else {
                let value = next(&mut rng).to_le_bytes().to_vec();
                snapshot.insert(key.clone(), value.clone());
                TabletMutation::Put { key, value }
            };
            let previous = history.len() as u64 - 1;
            let write = disk
                .prepare_write(1, previous, vec![mutation.clone()])
                .unwrap();
            disk.commit(write).unwrap();
            let write = memory.prepare_write(1, previous, vec![mutation]).unwrap();
            memory.commit(write).unwrap();
            history.push(snapshot);
            match next(&mut rng) % 4 {
                0 => disk.publish_checkpoint().unwrap(),
                1 => disk.checkpoint().unwrap(),
                2 => {
                    drop(disk);
                    disk = WalBackedTablet::open(descriptor.clone(), &path).unwrap();
                }
                _ => {}
            }
            assert_eq!(
                disk.current_sequence(),
                previous + 1,
                "seed={seed} step={step}"
            );
            for (seq, expected) in history.iter().enumerate() {
                for key in b'a'..=b'e' {
                    let key = [key];
                    let value = expected.get(key.as_slice()).map(Vec::as_slice);
                    assert_eq!(
                        disk.read_at(&key, seq as u64).unwrap(),
                        value,
                        "disk seed={seed} step={step} snapshot={seq} key={key:?}"
                    );
                    assert_eq!(
                        memory.read_at(&key, seq as u64).unwrap(),
                        value,
                        "memory seed={seed} step={step} snapshot={seq} key={key:?}"
                    );
                }
            }
        }
    }
}
