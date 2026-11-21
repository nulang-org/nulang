function add(x, y) {
  return x + y;
}

let sum = 0;
let i = 0;
while (i < 9_000_000) {
  sum = add(sum, i * 3 - Math.trunc(i / 7));
  if (sum > 1_000_000_000) {
    sum -= 1_000_000_000;
  }
  i += 1;
}
console.log(sum);
