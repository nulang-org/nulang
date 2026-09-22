fn main() {
    let mut sum: i64 = 0;
    let mut i: i64 = 0;
    while i < 9_000_000 {
        sum = sum + i * 3 - i / 7;
        i += 1;
    }
    println!("{sum}");
}
