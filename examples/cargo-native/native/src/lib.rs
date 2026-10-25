/// C-compatible adapter to crc32fast, safe for signed 64-bit integer arguments.
#[unsafe(no_mangle)]
pub extern "C" fn crc32_i64(value: i64) -> i64 {
    crc32fast::hash(&value.to_le_bytes()) as i64
}
