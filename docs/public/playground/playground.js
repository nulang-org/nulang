/* Nulang playground driver.
 *
 * Loads nulang_playground.wasm (compiler front-end + CoreVM) and talks to it
 * through its tiny C-style ABI — no wasm-bindgen glue needed:
 *
 *   nulang_alloc(len)        -> ptr   (scratch buffer for source text)
 *   nulang_free(ptr, len)             (release scratch buffer)
 *   nulang_run(ptr, len)     -> rptr  (length-prefixed JSON result:
 *                                      u32 LE byte length, then UTF-8 JSON
 *                                      { ok, output, error })
 */
"use strict";

const EXAMPLES = [
  {
    name: "Hello, Nulang!",
    src: `// Welcome to the Nulang playground!
perform IO.print("Hello, Nulang!")

let name = "World"
perform IO.print("Hello, " + name + "!")

let answer = 42
perform IO.print("The answer is " + perform Int.to_string(answer))
`,
  },
  {
    name: "Functions & recursion",
    src: `let rec fib = fn(n) {
    if n < 2 then n else fib(n - 1) + fib(n - 2)
}

let rec fact = fn(n) {
    if n <= 1 then 1 else n * fact(n - 1)
}

perform IO.print("fib(10) = " + perform Int.to_string(fib(10)))
perform IO.print("5! = " + perform Int.to_string(fact(5)))
`,
  },
  {
    name: "Closures",
    src: `let add = fn(x, y) { x + y }

let make_adder = fn(n) { fn(x) { add(x, n) } }
let add10 = make_adder(10)

perform IO.print("add10(32) = " + perform Int.to_string(add10(32)))

let result = {
    let a = 10
    let b = 20
    a + b
}
perform IO.print("Block result: " + perform Int.to_string(result))
`,
  },
  {
    name: "Arithmetic",
    src: `let a = 42
perform IO.print("42 = " + perform Int.to_string(a))
perform IO.print("10 - 3 = " + perform Int.to_string(10 - 3))
perform IO.print("6 * 7 = " + perform Int.to_string(6 * 7))
perform IO.print("84 / 2 = " + perform Int.to_string(84 / 2))
`,
  },
];

const editor = document.getElementById("editor");
const output = document.getElementById("output");
const runBtn = document.getElementById("run");
const statusEl = document.getElementById("status");
const exampleSel = document.getElementById("examples");

for (const [i, ex] of EXAMPLES.entries()) {
  const opt = document.createElement("option");
  opt.value = String(i);
  opt.textContent = ex.name;
  exampleSel.appendChild(opt);
}
exampleSel.addEventListener("change", () => {
  editor.value = EXAMPLES[Number(exampleSel.value)].src;
});
editor.value = EXAMPLES[0].src;

function setStatus(msg, cls) {
  statusEl.textContent = msg;
  statusEl.className = cls || "";
}

let wasm = null;

async function loadCompiler() {
  try {
    const resp = await fetch("nulang_playground.wasm");
    if (!resp.ok) throw new Error(`fetch failed: HTTP ${resp.status}`);
    const bytes = await resp.arrayBuffer();
    const { instance } = await WebAssembly.instantiate(bytes, {});
    wasm = instance.exports;
    if (
      typeof wasm.nulang_alloc !== "function" ||
      typeof wasm.nulang_run !== "function" ||
      typeof wasm.nulang_free !== "function"
    ) {
      throw new Error("wasm module is missing the nulang_* exports");
    }
    runBtn.disabled = false;
    setStatus("compiler ready", "ok");
  } catch (e) {
    setStatus("failed to load compiler: " + e.message, "err");
    output.className = "err";
    output.textContent =
      "Could not load nulang_playground.wasm.\n\n" +
      e.message +
      "\n\nIf you are serving this directory yourself, build the wasm first:\n" +
      "  playground/web/build.sh";
  }
}

function runSource(src) {
  const { memory, nulang_alloc, nulang_run, nulang_free } = wasm;
  const encoded = new TextEncoder().encode(src);
  const ptr = nulang_alloc(encoded.length);
  new Uint8Array(memory.buffer, ptr, encoded.length).set(encoded);
  const rptr = nulang_run(ptr, encoded.length);
  nulang_free(ptr, encoded.length);
  const len = new DataView(memory.buffer, rptr, 4).getUint32(0, true);
  const json = new TextDecoder().decode(
    new Uint8Array(memory.buffer, rptr + 4, len)
  );
  return JSON.parse(json);
}

runBtn.addEventListener("click", () => {
  if (!wasm) return;
  const t0 = performance.now();
  try {
    const result = runSource(editor.value);
    const ms = (performance.now() - t0).toFixed(1);
    if (result.ok) {
      output.className = "";
      output.textContent = result.output || "(program produced no output)";
      setStatus(`ran in ${ms} ms`, "ok");
    } else {
      output.className = "err";
      output.textContent = result.error || "unknown error";
      setStatus("error", "err");
    }
  } catch (e) {
    output.className = "err";
    output.textContent = "wasm trap: " + e.message;
    setStatus("trap", "err");
  }
});

// Ctrl/Cmd+Enter runs.
editor.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && !runBtn.disabled) {
    e.preventDefault();
    runBtn.click();
  }
});

loadCompiler();
