//! A paired, opt-in Rust-side comparison of read-only views and copied slices.
//! This does not benchmark the Nulang interpreter, effects, JIT, or WASM.

use nulang_numeric::NumericBuffer;
use std::hint::black_box;
use std::ops::Range;
use std::time::Instant;

fn sum_views(data: &NumericBuffer<i64>, range: Range<usize>, iterations: usize) -> i64 {
    let mut checksum = 0_i64;
    for _ in 0..iterations {
        let view = black_box(data.view(range.clone()).expect("benchmark range is valid"));
        let sum = view
            .as_slice()
            .iter()
            .copied()
            .fold(0_i64, i64::wrapping_add);
        checksum = checksum.wrapping_add(black_box(sum));
    }
    checksum
}

fn sum_copies(data: &NumericBuffer<i64>, range: Range<usize>, iterations: usize) -> i64 {
    let mut checksum = 0_i64;
    for _ in 0..iterations {
        let copy = black_box(data.as_slice()[range.clone()].to_vec());
        let sum = copy.iter().copied().fold(0_i64, i64::wrapping_add);
        checksum = checksum.wrapping_add(black_box(sum));
    }
    checksum
}

#[derive(Clone, Copy)]
struct Sample {
    elapsed_ns: u128,
    checksum: i64,
}

fn measure(f: impl FnOnce() -> i64) -> Sample {
    let start = Instant::now();
    let checksum = f();
    Sample {
        elapsed_ns: start.elapsed().as_nanos(),
        checksum,
    }
}

fn main() {
    // Match the same logical operation: sum a middle half of an owned buffer,
    // then discard the view/copy. These are *Rust primitive* costs, not Nulang
    // source-language timings. Pair on one host and alternate order to reduce
    // drift from CPU heat and scheduler contention.
    const CASES: [(usize, usize); 3] = [(128, 50_000), (8_192, 2_000), (65_536, 400)];
    const PAIRS: usize = 8;

    println!("size,range_len,iterations,round,mode,elapsed_ns,checksum");
    for (size, iterations) in CASES {
        let data = NumericBuffer::new(
            (0..size)
                .map(|i| (i as i64 % 31) - 15)
                .collect::<Vec<_>>(),
        );
        let range = size / 4..(size * 3) / 4;

        // Warm up both paths before the first measured pair.
        black_box(sum_views(&data, range.clone(), 100));
        black_box(sum_copies(&data, range.clone(), 100));

        for round in 0..PAIRS {
            let (view, copy) = if round.is_multiple_of(2) {
                (
                    measure(|| sum_views(&data, range.clone(), iterations)),
                    measure(|| sum_copies(&data, range.clone(), iterations)),
                )
            } else {
                let copy = measure(|| sum_copies(&data, range.clone(), iterations));
                let view = measure(|| sum_views(&data, range.clone(), iterations));
                (view, copy)
            };
            assert_eq!(
                view.checksum, copy.checksum,
                "both paths must compute identical results"
            );
            println!(
                "{size},{},{iterations},{round},view,{},{}",
                range.len(),
                view.elapsed_ns,
                view.checksum
            );
            println!(
                "{size},{},{iterations},{round},copy,{},{}",
                range.len(),
                copy.elapsed_ns,
                copy.checksum
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_checksums_for_copied_and_read_only_views() {
        let data = NumericBuffer::new(vec![1_i64, -2, 3, 4, -5]);
        let span = 1..4;
        let view_sum = sum_views(&data, span.clone(), 7);
        let copy_sum = sum_copies(&data, span, 7);
        assert_eq!(view_sum, copy_sum);
    }

    #[test]
    fn empty_ranges_do_not_read_out_of_bounds() {
        let data = NumericBuffer::new(vec![1_i64, 2, 3]);
        assert_eq!(sum_views(&data, 3..3, 6), 0);
        assert_eq!(sum_copies(&data, 3..3, 6), 0);
    }

    #[test]
    fn negative_values_and_repeated_iterations_preserve_checksum() {
        let data = NumericBuffer::new(vec![-8_i64, -2, 4, 5]);
        assert_eq!(sum_views(&data, 0..4, 3), -3);
        assert_eq!(sum_copies(&data, 0..4, 3), -3);
    }
}
