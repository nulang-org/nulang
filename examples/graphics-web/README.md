# Browser graphics interoperability pilot

This opt-in web example renders up to 10,000 points using **WebGL 2**, falling
back to **Canvas 2D** if WebGL 2 is unavailable. The HTML and accessibility
semantics remain in the DOM. No compiler extensions, GPU shader compiler,
WebGPU polyfill, external dependencies, or server round trips are introduced.

## Run

From this directory:

```sh
nula dev
```

Then open `http://127.0.0.1:3000/`. For a static build use `nula build --web`
and serve the resulting `dist/` directory. The Nulang web builder copies
`static/` into `dist/` and automatically injects `app.client.js` because the
page contains a `data-signal` binding. The module script runs after parsing,
so the `window.nulang.signal('pointCount')` bridge is available.

## Test

```sh
npm test
```

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
