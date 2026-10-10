#!/usr/bin/env bash
# Differential fuzzing campaign runner. Builds once and executes bounded batches
# in separate processes to release JIT/AOT allocations between batches.
# Usage: scripts/difffuzz.sh [--seeds N] [--seed-base N|0xN] [--time SECONDS]
#                             [--batch-size N] [--release] [--quiet]
set -euo pipefail

cd "$(dirname "$0")/.."

SEEDS=10000
SEED_BASE=0
BATCH_SIZE=800
TIME_SECS=""
PROFILE=debug
EXTRA=()

usage_error() { echo "difffuzz.sh: $*" >&2; exit 2; }
require_value() { (( $# >= 2 )) || usage_error "$1 requires a value"; }

while (( $# )); do
    case "$1" in
        --seeds|--seed-base|--batch-size|--time)
            require_value "$@"
            case "$1" in
                --seeds) SEEDS="$2" ;;
                --seed-base) SEED_BASE="$2" ;;
                --batch-size) BATCH_SIZE="$2" ;;
                --time) TIME_SECS="$2" ;;
            esac
            shift 2 ;;
        --release) PROFILE=release; shift ;;
        *) EXTRA+=("$1"); shift ;;
    esac
done

[[ "$SEEDS" =~ ^[0-9]+$ ]] || usage_error "--seeds must be a non-negative decimal integer"
[[ "$BATCH_SIZE" =~ ^[0-9]+$ ]] || usage_error "--batch-size must be a positive decimal integer"
[[ -z "$TIME_SECS" || "$TIME_SECS" =~ ^[0-9]+$ ]] || usage_error "--time must be a non-negative decimal integer"
[[ "$SEED_BASE" =~ ^[0-9]+$ || "$SEED_BASE" =~ ^0x[0-9a-fA-F]+$ ]] || usage_error "--seed-base must be decimal or 0x-prefixed hex"

# Compare as decimal strings before shell arithmetic so oversized values cannot
# wrap, turn negative or bypass bounds. u64 wrapping of seed starts is intentional.
within_u64() {
    local n="$1" bound="$2"
    n="${n#"${n%%[!0]*}"}"
    [[ -n "$n" ]] || n=0
    (( ${#n} < ${#bound} )) || { (( ${#n} == ${#bound} )) && [[ "$n" == "$bound" || "$n" < "$bound" ]]; }
}
within_u64 "$SEEDS" 9223372036854775807 || usage_error "--seeds exceeds supported range"
within_u64 "$BATCH_SIZE" 9223372036854775807 || usage_error "--batch-size exceeds supported range"
if [[ -n "$TIME_SECS" ]]; then
    within_u64 "$TIME_SECS" 9223372036854775807 || usage_error "--time exceeds supported range"
fi
if [[ "$SEED_BASE" == 0x* ]]; then
    (( ${#SEED_BASE} <= 18 )) || usage_error "--seed-base exceeds u64"
    SEED_START=$((16#${SEED_BASE:2}))
else
    within_u64 "$SEED_BASE" 18446744073709551615 || usage_error "--seed-base exceeds u64"
    SEED_START=$((10#$SEED_BASE))
fi

(( 10#$BATCH_SIZE > 0 )) || usage_error "--batch-size must be positive"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/ct-bfuzz}"
export NULANG_STDLIB="${NULANG_STDLIB:-${PWD}/src/stdlib}"
mkdir -p fuzz/differential/crashers

build_flags=()
[[ "$PROFILE" == release ]] && build_flags+=(--release)
cargo build "${build_flags[@]}" --no-default-features --features difffuzz --bin nula_difffuzz

BIN="${CARGO_TARGET_DIR}/${PROFILE}/nula_difffuzz"
remaining=$((10#$SEEDS))
completed=0
batch_limit=$((10#$BATCH_SIZE))
start_time=$SECONDS
while (( remaining > 0 )); do
    batch=$batch_limit
    (( batch <= remaining )) || batch=$remaining
    time_args=()
    if [[ -n "$TIME_SECS" ]]; then
        seconds_left=$((10#$TIME_SECS - (SECONDS - start_time)))
        (( seconds_left > 0 )) || break
        time_args=(--time "$seconds_left")
    fi

    # printf %u converts Bash's signed 64-bit arithmetic to an unsigned u64.
    printf -v current_seed '%u' "$((SEED_START + completed))"
    echo "difffuzz.sh: seed-base=$current_seed seeds=$batch remaining=$remaining" >&2
    "$BIN" --seeds "$batch" --seed-base "$current_seed" "${time_args[@]}" \
        --crashers fuzz/differential/crashers "${EXTRA[@]}"
    completed=$((completed + batch))
    remaining=$((remaining - batch))
done

echo "difffuzz.sh: complete; scheduled $completed of $SEEDS seeds" >&2
