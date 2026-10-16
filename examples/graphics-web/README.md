# Browser graphics interoperability pilot

This opt-in web example renders up to 10,000 points using **WebGL 2**, falling
back to **Canvas 2D** if WebGL 2 is unavailable. The HTML and accessibility
semantics remain in the DOM. No compiler extensions, GPU shader compiler,
WebGPU polyfill, external dependencies, or server round trips are introduced.

## Run

From this directory, using a built Nulang CLI (or an installed `nula` launcher):

```sh
../../target/release/nulang nula dev
```

Then open `http://127.0.0.1:3000/`. For a static build use `../../target/release/nulang nula build --web`
and serve the resulting `dist/` directory. The Nulang web builder copies
`static/` into `dist/` and automatically injects `app.client.js` because the
page contains a `data-signal` binding. The module script runs after parsing,
so the `window.nulang.signal('pointCount')` bridge is available.

## Test

```sh
npm test
```

CI also runs the Node suite and builds this example with the Nulang release CLI, checking that the generated HTML and static assets exist.

### Strict WebGL 2 browser verification

Run on a machine with Chromium and WebGL 2 support **after** `nula build --web`:

```sh
npm install --no-save --package-lock=false playwright-core
CHROMIUM_EXECUTABLE=/usr/bin/chromium npm run test:browser:webgl
```

Set `CHROMIUM_EXECUTABLE` to your actual browser binary. This diagnostic serves
the compiler-produced `dist/` output, verifies Nulang signal hydration and
slider updates, requires a **real WebGL 2 browser context** (Canvas 2D fallback
is a failure), and checks for graphics and console errors. It is deliberately
optional: CI runs the headless-independent unit tests and Nulang build while
hardware-specific GPU validation remains an explicit review gate. Headless
Chromium or sandboxed hosts may expose no WebGL 2 context; report this as
*not verified*, not a passing GPU test. This is a functional smoke test, not a
GPU benchmark or a guarantee of hardware acceleration.


Requires Node 20+. These tests exercise WebGL 2 mock lifecycle, context
loss/restoration, Canvas 2D fallback, frame coalescing, GPU resource cleanup,
invalid geometry, and the actual generated client-signal JavaScript embedded
in `src/web/reactivity.rs`.

## Implementation constraints

- WebGL 2 is preferred; Canvas 2D is used only when WebGL 2 context creation
  returns `null`. Shader compilation/link failure is surfaced rather than
  attempting a second context type on the same canvas.
- `createPointRenderer(canvas)` accepts interleaved `Float32Array` pairs in
  normalized device coordinates (both axes in `[-1, 1]`). The caller owns the
  array and must not mutate it until the next update. One GPU vertex buffer is
  reused; frames are coalesced via `requestAnimationFrame`.
- Reactive updates stay in the browser and do **not** send actor messages.
- The example inserts the *constant* `<script type="module" src="/demo.js"></script>` via `Web.raw`: Nulang correctly blocks script tags constructed through `Web.html` for security. Do not interpolate user-controlled strings into this trusted-only escape hatch.
- `dispose()` cancels scheduled frames, disconnects its resize observer, and
  deletes GPU resources. WebGL context loss pauses rendering; restoration
  recreates the program and buffer from the retained points.
- If no browser graphics context is available, the demo renders an accessible
  error message. The `<canvas>` has a textual label and DOM controls remain
  keyboard-accessible.

This is deliberately an **example-level browser adapter**, not a registered
`@nulang/graphics` package or a production graphics framework. Before
promoting it, verify a real Nulang `nula dev` browser smoke test on desktop
and mobile, benchmark 2D/WebGL render times and total bundle size, and review
whether a reusable browser-interoperability package is warranted.
