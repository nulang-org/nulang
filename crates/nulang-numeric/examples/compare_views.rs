//! A paired, opt-in Rust-side comparison of read-only views and copied slices.
//! This does not benchmark the Nulang interpreter, effects, JIT, or WASM.

use nulang_numeric::NumericBuffer;
use std::ops::Range;
use std::hint::black_box;
use std::time::Instant;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_checksums_for_copied_and_read_only_views() {
        let data = NumericBuffer::new(vec![1_i64, -2, 3, 4, -5]);
        let span = 1..4;
        assert_eq!(sum_views(&data, span.clone(), 7), sum_copies(&data, span, 7));
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

fn main() {}
