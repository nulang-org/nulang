fn add(x: i64, y: i64) -> i64 {
    x + y
}

fn main() {
    let mut sum: i64 = 0;
    let mut i: i64 = 0;
    while i < 9_000_000 {
        sum = add(sum, i * 3 - i / 7);
        if sum > 1_000_000_000 {
            sum -= 1_000_000_000;
        }
        i += 1;
    }
    println!("{sum}");
}
