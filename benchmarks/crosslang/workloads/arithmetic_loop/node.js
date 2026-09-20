let sum = 0;
let i = 0;
while (i < 9_000_000) {
  sum = sum + i * 3 - Math.trunc(i / 7);
  i += 1;
}
console.log(sum);
