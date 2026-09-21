# Deploying the Nulang Playground to nulang.org/playground

The browser playground is a static bundle: HTML, CSS, JavaScript, and a WASM
build of the real compiler frontend + CoreVM. No application server is
required.

## Production deployment

Production deployment is automated by
`.github/workflows/docs-sync.yml` whenever compiler, docs, or playground
sources change on `main`.

The workflow:

1. Installs Rust 1.95.0 with the `wasm32-unknown-unknown` target.
2. Runs `playground/web/build.sh`.
3. Runs `docs/scripts/sync-playground.mjs`, which copies the browser bundle to
   `docs/public/playground/`.
4. Builds the Astro documentation site as a validation gate.
5. Commits the generated `docs/public/playground/` bundle back to `main`
   with a normal deployable commit.
6. Cloudflare Pages deploys that generated revision, including
   `/playground/index.html` and `/playground/nulang_playground.wasm`.

The production Pages build fails closed if the generated WASM does not yet
exist. That means the source merge cannot briefly publish an interactive hero
or playground shell that points at a missing compiler; the previous successful
deployment remains live until Docs Sync commits the generated artifact.

The generated WASM is intentionally not maintained by hand. The workflow
refreshes it from compiler sources so the hosted playground cannot silently
drift from the repository implementation.

## Local browser-playground build

```bash
rustup target add wasm32-unknown-unknown --toolchain 1.95.0
playground/web/build.sh

cd playground/web
python3 -m http.server 8080
# open http://localhost:8080
```

The build produces:

```text
playground/web/
├── index.html
├── playground.js
├── style.css
└── nulang_playground.wasm
```

The bundle uses relative asset paths, so the same files work at
`/playground/` or at a local server root.

## Docs-only builds

`pnpm run build` inside `docs/` runs `docs/scripts/sync-playground.mjs`
before Astro. The script always refreshes the HTML/CSS/JS shell from
`playground/web/`.

If `playground/web/nulang_playground.wasm` exists, it refreshes the public
WASM too. Otherwise it preserves an already-generated
`docs/public/playground/nulang_playground.wasm`. Lightweight local and preview
builds may continue without that artifact, but the production Cloudflare Pages
build on `main` refuses to deploy when it is missing. The authoritative
rebuild happens in Docs Sync on `main`.

## Verification after deployment

```bash
curl -sI https://nulang.org/playground/
curl -sI https://nulang.org/playground/nulang_playground.wasm
```

Expected behavior:

- both requests return `200`;
- the WASM asset is served as `application/wasm`;
- the playground status changes to **compiler ready**;
- running the default example prints:

```text
Hello, Nulang!
Hello, World!
The answer is 42
```

The browser playground intentionally targets the CoreVM subset. Actors,
networking, FFI, and JIT execution remain native-runtime features. The older
server-side playground (`playground/server.py`) is still useful when testing
those broader capabilities locally.
