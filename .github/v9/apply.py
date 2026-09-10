#!/usr/bin/env python3
from pathlib import Path

def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))

split_brain_script = r'''#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

TEST_NAME="runtime::tests::test_five_node_cluster_split_brain_detects_and_heals"
ITERATIONS="${NULANG_SPLIT_BRAIN_ITERATIONS:-5}"
TIMEOUT_SECONDS="${NULANG_SPLIT_BRAIN_TIMEOUT_SECONDS:-90}"

PROFILE_ARGS=()
if [[ "${1:-}" == "--release" ]]; then
  PROFILE_ARGS+=(--release)
  shift
fi
if [[ $# -ne 0 ]]; then
  echo "usage: $0 [--release]" >&2
  exit 2
fi

if ! [[ "$ITERATIONS" =~ ^[1-9][0-9]*$ ]]; then
  echo "error: NULANG_SPLIT_BRAIN_ITERATIONS must be a positive integer" >&2
  exit 2
fi
if ! [[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]]; then
  echo "error: NULANG_SPLIT_BRAIN_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi

if ! cargo test --lib "${PROFILE_ARGS[@]}" -- --list |
  grep -Fxq "${TEST_NAME}: test"
then
  echo "error: expected split-brain regression test is missing: $TEST_NAME" >&2
  exit 1
fi

run_one() {
  local iteration="$1"
  echo "=== split-brain stability pass ${iteration}/${ITERATIONS} ==="
  local cmd=(
    cargo test --lib "${PROFILE_ARGS[@]}" "$TEST_NAME" --
    --exact --nocapture --test-threads=1
  )
  if command -v timeout >/dev/null 2>&1; then
    timeout --signal=TERM --kill-after=10s "${TIMEOUT_SECONDS}s" "${cmd[@]}"
  else
    "${cmd[@]}"
  fi
}

for ((i = 1; i <= ITERATIONS; i++)); do
  run_one "$i"
done

echo "PASS: ${TEST_NAME} passed ${ITERATIONS} consecutive isolated runs"
'''
sp = Path("scripts/ci/test_split_brain_stability.sh")
sp.parent.mkdir(parents=True, exist_ok=True)
if sp.exists():
    raise SystemExit(f"{sp}: already exists; refusing to overwrite")
sp.write_text(split_brain_script)
sp.chmod(0o755)

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Test
        run: |
          # The five-node split-brain cluster test is known to be timing-
          # sensitive on GitHub Actions shared runners. Retry the whole suite
          # once on failure so a single flaky run does not block merges.
          cargo test || (echo "Retrying test suite after flaky failure..." && cargo test)
''',
    '''      - name: Test
        run: cargo test -- --skip test_five_node_cluster_split_brain_detects_and_heals

      - name: Five-node split-brain stability
        timeout-minutes: 10
        env:
          NULANG_SPLIT_BRAIN_ITERATIONS: "5"
          NULANG_SPLIT_BRAIN_TIMEOUT_SECONDS: "90"
        run: scripts/ci/test_split_brain_stability.sh
''',
)

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Generate coverage report
        run: cargo llvm-cov --all-features --lcov --output-path lcov.info
''',
    '''      - name: Generate coverage report
        run: |
          cargo llvm-cov --all-features --lcov --output-path lcov.info -- \
            --skip test_five_node_cluster_split_brain_detects_and_heals
''',
)

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Test (release)
        run: cargo test --release
''',
    '''      - name: Test (release)
        run: |
          cargo test --release -- \
            --skip test_five_node_cluster_split_brain_detects_and_heals

      - name: Five-node split-brain stability (release)
        timeout-minutes: 5
        env:
          NULANG_SPLIT_BRAIN_ITERATIONS: "2"
          NULANG_SPLIT_BRAIN_TIMEOUT_SECONDS: "90"
        run: scripts/ci/test_split_brain_stability.sh --release
''',
)

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Test
        run: cargo test --features wasm-backend
''',
    '''      - name: Test
        run: |
          cargo test --features wasm-backend -- \
            --skip test_five_node_cluster_split_brain_detects_and_heals
''',
)

replace_once(
    "src/dst.rs",
    '''pub fn dst_seed_count(default: u64) -> u64 {
    std::env::var("NULANG_DST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}
''',
    '''pub fn dst_seed_count(default: u64) -> u64 {
    std::env::var("NULANG_DST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// Deterministic seed stream for DST sweeps.
///
/// Local/per-PR runs keep the historical base of zero. Scheduled CI can set
/// `NULANG_DST_SEED_BASE` to explore a new reproducible window.
pub fn dst_seeds(default_count: u64) -> impl Iterator<Item = u64> {
    let count = dst_seed_count(default_count);
    let base = std::env::var("NULANG_DST_SEED_BASE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (0..count).map(move |offset| base.wrapping_add(offset))
}
''',
)

for path, old_count in [("src/runtime/tests.rs", 2000), ("src/runtime/tests.rs", 60)]:
    replace_once(
        path,
        f'''    let seeds = crate::dst::dst_seed_count({old_count});

    for seed in 0..seeds {{
''',
        f'''    for seed in crate::dst::dst_seeds({old_count}) {{
''',
    )

for old_count in [50, 30, 20, 25]:
    replace_once(
        "src/runtime/cluster_dst.rs",
        f'''        let seeds = crate::dst::dst_seed_count({old_count});

        for seed in 0..seeds {{
''',
        f'''        for seed in crate::dst::dst_seeds({old_count}) {{
''',
    )

replace_once(
    ".github/workflows/dst-nightly.yml",
    '''  workflow_dispatch: {} # allow manual runs for investigation

env:
  CARGO_TERM_COLOR: always
''',
    '''  workflow_dispatch: {} # allow manual runs for investigation

permissions:
  contents: read

concurrency:
  group: dst-seed-sweeps-nightly
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: "1"
''',
)
replace_once(
    ".github/workflows/dst-nightly.yml",
    '''    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
''',
    '''    runs-on: ubuntu-latest
    timeout-minutes: 120
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
''',
)
replace_once(
    ".github/workflows/dst-nightly.yml",
    '''      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
''',
    '''      - name: Install Rust 1.95.0
        uses: dtolnay/rust-toolchain@46817827a5bfabe028bf34e1cce71fd40e2ff697 # 1.95.0
''',
)
replace_once(
    ".github/workflows/dst-nightly.yml",
    '''      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
''',
    '''      - name: Cache cargo
        uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
''',
)
replace_once(
    ".github/workflows/dst-nightly.yml",
    '''      - name: Run DST seed sweeps
        env:
          NULANG_DST_SEEDS: "10000"
        run: cargo test --lib dst_
''',
    '''      - name: Run DST seed sweeps
        shell: bash
        run: |
          set -euo pipefail
          set -o pipefail
          COUNT=10000
          UTC_DAY=$(( $(date -u +%s) / 86400 ))
          BASE=$(( UTC_DAY * COUNT ))
          LOG="/tmp/nulang-dst-seed-sweep.log"
          export NULANG_DST_SEEDS="$COUNT"
          export NULANG_DST_SEED_BASE="$BASE"
          {
            echo "DST seed campaign"
            echo "count=$COUNT"
            echo "base=$BASE"
            echo "last=$((BASE + COUNT - 1))"
            echo "reproduce: NULANG_DST_SEEDS=1 NULANG_DST_SEED_BASE=<seed> cargo test --lib <test> -- --nocapture"
          } | tee "$LOG"
          cargo test --lib dst_ -- --nocapture 2>&1 | tee -a "$LOG"

      - name: Upload DST failure log
        if: failure()
        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7
        with:
          name: dst-seed-sweep-failure
          path: /tmp/nulang-dst-seed-sweep.log
          if-no-files-found: error
          retention-days: 30
''',
)

replace_once(
    "src/format/nbc.rs",
    '''        let meta_off = meta_len_off + 4;
        if bytes.len() < meta_off + meta_len {
            return Err(FormatError::LengthMismatch {
                declared: meta_len as u32,
                actual: bytes.len() - meta_off,
            });
        }
''',
    '''        let meta_off = meta_len_off + 4;
        let actual_meta_len = bytes.len() - meta_off;
        if actual_meta_len != meta_len {
            return Err(FormatError::LengthMismatch {
                declared: meta_len as u32,
                actual: actual_meta_len,
            });
        }
''',
)

nbc_tests = r'''
    #[test]
    fn test_nbc_rejects_trailing_bytes_after_metadata() {
        let mut bytes = sample_module().to_nbc(None).unwrap();
        let instr_count = u32::from_be_bytes(bytes[44..48].try_into().unwrap()) as usize;
        let meta_len_off = NBC_HEADER_LEN + instr_count * 4;
        let declared_meta_len =
            u32::from_be_bytes(bytes[meta_len_off..meta_len_off + 4].try_into().unwrap());
        bytes.extend_from_slice(b"hidden-trailer");
        let err = CodeModule::from_nbc(&bytes).unwrap_err();
        assert_eq!(
            err,
            FormatError::LengthMismatch {
                declared: declared_meta_len,
                actual: declared_meta_len as usize + b"hidden-trailer".len(),
            }
        );
    }

    #[test]
    fn test_nbc_decoder_never_panics_on_deterministic_corruptions() {
        let base = sample_module().to_nbc(Some([0xA5; 32])).unwrap();
        let mut rng = 0x4E42_435F_4655_5A5Au64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for case in 0..10_000usize {
            let mut bytes = base.clone();
            match next() % 5 {
                0 => {
                    let idx = (next() as usize) % bytes.len();
                    bytes[idx] ^= (next() as u8) | 1;
                }
                1 => bytes.truncate((next() as usize) % (bytes.len() + 1)),
                2 => {
                    let count = 1 + (next() as usize % 16);
                    for _ in 0..count { bytes.push(next() as u8); }
                }
                3 => {
                    let idx = (next() as usize) % (bytes.len() - 3);
                    bytes[idx..idx + 4].copy_from_slice(&(next() as u32).to_be_bytes());
                }
                _ => {
                    let idx = (next() as usize) % (bytes.len() + 1);
                    bytes.insert(idx, next() as u8);
                }
            }
            let outcome = std::panic::catch_unwind(|| CodeModule::from_nbc(&bytes));
            assert!(
                outcome.is_ok(),
                "from_nbc panicked on corruption case {case}, len={}",
                bytes.len()
            );
        }
    }

'''
replace_once(
    "src/format/nbc.rs",
    '''    #[test]
    fn test_nbc_rejects_non_finite_float_constant() {
''',
    nbc_tests + '''    #[test]
    fn test_nbc_rejects_non_finite_float_constant() {
''',
)

wire_test = r'''
    #[test]
    fn test_packet_decoder_never_panics_on_deterministic_corruptions() {
        let packet = Packet::ActorMessage {
            target_actor: 42,
            behavior_name: "handle_msg".to_string(),
            content_hash: Some([0xA5; 32]),
            payload: vec![Value::int(123), Value::string(0), Value::object(0)],
            string_table: vec!["hello".to_string()],
            object_table: vec![(0, vec![1, 2, 3, 4, 5, 6, 7, 8])],
            sender_actor: 99,
            sender_node: NodeId(0xDEAD_BEEF_CAFE_BABE),
            priority: MessagePriority::Normal,
            trace_id: Some(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
            ),
        };
        let base = packet.to_bytes(0x1234);
        let mut rng = 0x4E55_4C30_4655_5A5Au64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for case in 0..5_000usize {
            let mut bytes = base.clone();
            match next() % 6 {
                0 => bytes[4] = (next() % 32) as u8,
                1 => {
                    let idx = (next() as usize) % bytes.len();
                    bytes[idx] ^= (next() as u8) | 1;
                }
                2 => bytes.truncate((next() as usize) % (bytes.len() + 1)),
                3 => {
                    let idx = (next() as usize) % (bytes.len() + 1);
                    bytes.insert(idx, next() as u8);
                }
                4 => {
                    if bytes.len() > PACKET_HEADER_LEN {
                        let idx = PACKET_HEADER_LEN
                            + (next() as usize % (bytes.len() - PACKET_HEADER_LEN));
                        bytes[idx] = next() as u8;
                    }
                }
                _ => {
                    let count = 1 + (next() as usize % 8);
                    for _ in 0..count { bytes.push(next() as u8); }
                }
            }
            let outcome = std::panic::catch_unwind(|| Packet::from_bytes(&bytes));
            assert!(
                outcome.is_ok(),
                "Packet::from_bytes panicked on corruption case {case}, len={}",
                bytes.len()
            );
        }
    }

'''
replace_once(
    "src/runtime/network.rs",
    '''    // ------------------------------------------------------------------
    // 2b. ActorMessage string table roundtrip
''',
    wire_test + '''    // ------------------------------------------------------------------
    // 2b. ActorMessage string table roundtrip
''',
)

changelog = Path("CHANGELOG.md")
text = changelog.read_text()
bullets = '''- **Reliability gates and frozen-decoder hardening.** Protected CI no longer
  retries the entire test suite; the five-node real-TCP split-brain scenario
  runs as an isolated consecutive-pass gate. Nightly DST rotates reproducible
  seed windows instead of replaying seed zero. `.nbc` v1 rejects trailing bytes
  after declared metadata, and deterministic corruption sweeps exercise both
  `.nbc` and NUL0 decoders as no-panic untrusted-input boundaries.
'''
heading = "### Fixed since 1.0.0-frozen — 2026-09-10\n\n"
if heading in text:
    if bullets not in text:
        text = text.replace(heading, heading + bullets, 1)
else:
    anchor = '''*Breaking changes require an accepted RFC and a deprecation cycle of at least
two major versions.*

'''
    if text.count(anchor) != 1:
        raise SystemExit("CHANGELOG.md: stable-tier anchor not unique")
    text = text.replace(anchor, anchor + heading + bullets + "\n", 1)
changelog.write_text(text)

print("Reliability v9 source transformation completed")
