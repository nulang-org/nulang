// Node smoke test for the playground wasm module.
//
// Usage:
//   playground/web/build.sh          # build the wasm first
//   node playground/web/test/smoke.mjs
//
// Exercises the same C-style ABI the browser driver uses and checks that
// real Nulang programs compile and run with expected output, and that a
// broken program reports a compile error instead of trapping.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { homepageExamples } from "../../../docs/src/lib/homepage-examples.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const wasmPath = path.join(here, "..", "nulang_playground.wasm");

if (!fs.existsSync(wasmPath)) {
  console.error(`missing ${wasmPath} — run playground/web/build.sh first`);
  process.exit(1);
}

const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {});
const { memory, nulang_alloc, nulang_run, nulang_free } = instance.exports;

function run(src) {
  const encoded = new TextEncoder().encode(src);
  const ptr = nulang_alloc(encoded.length);
  new Uint8Array(memory.buffer, ptr, encoded.length).set(encoded);
  const rptr = nulang_run(ptr, encoded.length);
  nulang_free(ptr, encoded.length);
  const len = new DataView(memory.buffer, rptr, 4).getUint32(0, true);
  const json = new TextDecoder().decode(new Uint8Array(memory.buffer, rptr + 4, len));
  return JSON.parse(json);
}

let failures = 0;
function check(name, src, expect) {
  const result = run(src);
  const pass = expect(result);
  console.log(`${pass ? "PASS" : "FAIL"}  ${name}`);
  if (!pass) {
    failures++;
    console.log("     got:", JSON.stringify(result));
  }
}

check(
  "IO.print output",
  'perform IO.print("Hello, Nulang!")\n',
  (r) => r.ok && r.output === "Hello, Nulang!\n"
);

check(
  "recursion (fib)",
  `let rec fib = fn(n) { if n < 2 then n else fib(n - 1) + fib(n - 2) }
perform IO.print(perform Int.to_string(fib(10)))
`,
  (r) => r.ok && r.output === "55\n"
);

check(
  "string concat + Int.to_string",
  'let a = 40\nlet b = 2\nperform IO.print("sum=" + perform Int.to_string(a + b))\n',
  (r) => r.ok && r.output === "sum=42\n"
);

check(
  "compile error is reported, not trapped",
  "perform IO.print(\n",
  (r) => !r.ok && r.error.length > 0
);

check(
  "name error is reported, not trapped",
  "perform IO.print(undefined_variable_xyz)\n",
  (r) => !r.ok && r.error.length > 0
);


// Homepage demos must run on the real compiler/CoreVM, not merely resemble
// snippets in the standalone playground. These output assertions detect
// compiler/runtime regressions before the docs site is published.
const homepageOutputs = new Map([
  ["hello", "Hello, Nulang!\nHello, World!\nThe answer is 42\n"],
  ["recursion", "fib(10) = 55\n5! = 120\n"],
  ["closures", "add10(32) = 42\nBlock result: 30\n"],
]);
for (const example of homepageExamples) {
  const expected = homepageOutputs.get(example.id);
  if (expected === undefined) {
    failures++;
    console.error("FAIL  unrecognized homepage sample:", example.id);
    continue;
  }
  check(
    "homepage " + example.id,
    example.code,
    (r) => r.ok && r.output === expected
  );
}
if (homepageExamples.length !== homepageOutputs.size) {
  failures++;
  console.error("FAIL  homepage example count does not match expected outputs");
}

// Run twice through the same instance: the result buffer must be reusable.
const again = run('perform IO.print("again")\n');
if (again.ok && again.output === "again\n") {
  console.log("PASS  result buffer reuse across calls");
} else {
  failures++;
  console.log("FAIL  result buffer reuse across calls — got:", JSON.stringify(again));
}

if (failures) {
  console.error(`\n${failures} check(s) failed`);
  process.exit(1);
}
console.log("\nall smoke checks passed");
