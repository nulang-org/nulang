# Hotwire Turbo Frames (experimental)

Nulang can render [Hotwire Turbo Frames](https://turbo.hotwired.dev/handbook/frames)
using its existing typed `Html` and `Web.html` host renderer. This feature
does **not** introduce a second DOM runtime or add a new HTTP host effect.

## Helpers

Import `stdlib::web::frames` for these four helpers:

| Helper | Purpose |
| --- | --- |
| `frame(id, children)` | Render an inline `<turbo-frame id="...">` |
| `lazy_frame(id, src)` | Render a frame that fetches `src` when visible |
| `eager_frame(id, src)` | Render a frame that fetches `src` immediately |
| `targeted_frame(id, target, children)` | Configure the default target for links inside a frame |

Example:

```nulang
import stdlib::web::frames

fn activity() {
    frame("activities", [perform Web.text("Tea & toast")])
}

app "activity" {
    route "GET" "/activity" -> activity
}
```

This produces a normal HTML response containing:

```html
<turbo-frame id="activities">Tea &amp; toast</turbo-frame>
```

For a lazily loaded section:

```nulang
let recent = lazy_frame("recent", "/recent")
```

The resulting frame has `id="recent"`, `src="/recent"`, and
`loading="lazy"`. The `GET /recent` response should include a
`<turbo-frame id="recent">...</turbo-frame>` element. The browser's
Turbo runtime extracts the matching frame and replaces only that region.
Responses are regular `text/html`; **not** Turbo Stream MIME.

## Client requirements and progressive enhancement

Serve the official Turbo JavaScript bundle from the same origin and load it
in the application's HTML layout; the experimental Nulang Web dev server's
CSP normally restricts scripts to `'self'`. This module generates the
standard HTML tag but **does not bundle or load Turbo**. With JavaScript
disabled, ordinary links and forms should still work via full-page
navigation, and the server must provide a valid whole-page fallback.

Use a stable, unique DOM ID for each frame. Avoid putting independently
managed Nulang signal DOM regions inside a Turbo-owned frame without a
clear ownership boundary.

Security considerations:
- `Web.text` escapes dynamic text/attribute values; existing `Web.html`
  host serialization rejects active content URLs such as
  `javascript:...`. Do not pass user-controlled markup through `Web.raw`.
- Authorize the initial page **and each frame endpoint** separately.
  Turbo Frame requests are ordinary HTTP requests; their `Turbo-Frame`
  header is not proof of identity or authorization.
- Baseline Web may return HTTP 200 with a `nil` body when the HTML
  serializer rejects an element. Draft PR #2583 fixes this to return HTTP
  500 instead; the test temporarily covers both shapes until #2583 merges.
  That is a server rendering error, not a user-facing validation response.
  Typed response/error handling, CSRF and production ingress hardening
  remain separate open Web work.
- Frames and Turbo Streams are independent interoperable surfaces.
  The Turbo Streams adapter is proposed in PR #2576; Frames do not depend
  on that PR.

## Tests

```bash
cargo test --locked --no-default-features --test web_turbo_frames
cargo test --locked --no-default-features --lib
```

The tests cover import resolution, typechecking, real HTTP rendering,
escaping of hostile HTML, rejection of unsafe URL schemes, and the
actual imported helper executing over HTTP. Browser automation with the
official Turbo client is still needed before production claims.
