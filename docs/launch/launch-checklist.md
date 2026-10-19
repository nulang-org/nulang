# Nulang Launch Checklist

Sequenced from repo state as of this writing. Work top-down; don't announce
until the gates in Phase 0 pass. Each item references where the real
instructions/state live.

## Phase 0 — Repo readiness (T-7 to T-3 days)

- [ ] **Merge or explicitly defer pending PRs.** Correctness, durability,
  security, and CI/release fixes targeting `main` must be merged or explicitly
  deferred with a documented reason before launch. Performance and experimental
  feature stacks may remain open only when they are not required for the
  supported release surface.
- [ ] **Green CI on the exact `main` commit to be tagged.** Treat every
  blocking build/test/lint/audit/docs/formal job as a release gate. Do not use
  historical counts or `docs/STATUS.md` as evidence that current `main` is
  green; that file is a dated infrastructure snapshot.
- [ ] **Run RELEASE_CHECKLIST.md + docs/RELEASING.md and cut the first
  release** (suggest `v0.1.0`): bump `Cargo.toml` version + `VERSION` in
  `src/main.rs`, add CHANGELOG entry, `git tag v0.1.0 && git push origin
  v0.1.0`, watch the Release workflow, verify all 8 artifacts (4 tarballs +
  4 checksums), and smoke-test one tarball per docs/RELEASING.md §4. Until
  this lands, the README install section points at a promise.
- [ ] **README sanity pass against reality** — verify feature/status claims
  against the tagged commit and current CI, avoid brittle exact test-count claims,
  and verify every README link resolves.
- [ ] **Merge this launch kit** (`docs/launch/`) to main.
- [ ] **Demo is executed and current** — `docs/launch/demo-script.md` was run
  against a real build and its observed output is recorded; the
  durable-recovery narrative was deliberately replaced with crash containment
  + honesty beat after execution showed supervised restarts return fresh
  state. Re-run the two demo programs before recording in case behavior
  changed on main (update the recorded output if so).
- [ ] **Record the demo** (asciinema, 60–90 s) and produce the GIF; add it
  to the README above the fold.

## Phase 1 — Surfaces (T-3 to T-1 days)

- [ ] **VS Code extension**: `cd editors/vscode && npm install && npx
  @vscode/vsce package` → attach `nulang-0.1.0.vsix` to the GitHub release.
  Marketplace publication is optional for launch day; a `.vsix` on the
  release plus the README's manual-install steps is enough.
- [ ] **Playground**: local version works via `python3 playground/server.py`
  (README documents it). Hosted nulang.org/playground is "coming soon" —
  either deploy it before launch (Cloudflare config exists: `wrangler.toml`,
  `registry-worker/`) or make sure every post links the local-run
  instructions, not the dead hosted URL. Branch
  `fix/playground-backend-and-sandboxing` exists — check its status before
  promising anything hosted.
- [ ] **Registry seeding**: follow `docs/REGISTRY_SEEDING.md` — publish at
  least the priority wave (`json`, `json-ext`, `http-client`, `test-utils`)
  with `nula publish` against `nulang registry serve`. (Note: there is no
  `packages/` directory on main yet; seed packages live on in-progress work
  or must be created per that doc.) If the wave isn't ready, launch anyway —
  but then no post may claim a working registry.
- [ ] **Staging check**: fresh-clone build on Linux *and* macOS from the
  tagged release; run the 17 `examples/` against the release binary.

## Phase 2 — Launch day

Posting times (US East Coast anchors; aim Tue–Thu):

- **Show HN**: 8:00–9:00 am ET. HN's front page cycles ~day-long; weekday
  mornings maximize the first-hour upvote window. Post link to the repo
  (or the release), then immediately add the author comment from
  `docs/launch/hn-post.md`.
- **lobste.rs**: same morning, ~30 min after HN. Link post, technical first
  comment from `docs/launch/lobsters-post.md`.
- **r/programminglanguages**: text post from
  `docs/launch/r-programminglanguages-post.md`, late morning ET.
- **ElixirForum**: `docs/launch/elixirforum-post.md`, early afternoon ET —
  after the HN thread's shape is known, so the post can preempt whatever the
  day has surfaced.
- Cross-post discipline: each post is tailored (done — see the files); do
  not paste the HN comment into ElixirForum. Link between threads only if
  asked.

## Phase 3 — First 48 hours (engagement plan)

- [ ] **Hour 0–4**: maintainer stays on HN; answer every comment. Use the
  pre-written answers in `hn-post.md` and `faq.md` as bases, personalize
  each. Upvote-worthy candor beats defensiveness — the "what's not real yet"
  list is an asset.
- [ ] **Hour 4–24**: check lobste.rs and Reddit every 2–3 h; ElixirForum
  twice. Convert repeated questions into FAQ additions live (commit to
  `docs/launch/faq.md`, reply with the link).
- [ ] **Triage inbound issues**: label within 24 h; anything reporting a doc
  claim that doesn't reproduce is P0 — fix the doc or the bug, then reply.
- [ ] **Do not** argue about: AI assistance (answer once, link faq.md),
  "why not Erlang" (answer once per thread, link), star counts, or language
  wars. One calm reply, then disengage.
- [ ] **Hour 24–48**: write a short "launch retro" comment in the HN thread
  and on ElixirForum summarizing what broke, what's fixed, and the top
  requested features. File the roadmap issues for the top asks.
- [ ] **Metrics to watch**: release downloads, repo stars, playground runs
  (if hosted), `nula` registry publishes, new issues/discussions opened.

## Kill criteria (postpone launch if any is true)

- CI red on main, or the tagged release artifacts fail smoke test.
- The demo's observed output diverges from the documented expected output
  and can't be fixed honestly in 48 h.
- Any README/FAQ claim is found to be unverifiable from the repo.
