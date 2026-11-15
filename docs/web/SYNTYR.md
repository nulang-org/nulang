# Optional Syntyr CSS integration for Nulang Web

Syntyr is a stylesheet preset, **not** a Nulang language/runtime dependency.
The HTTP router, rendering, request handling, and generated browser runtime
do not import or initialize Syntyr.

## Quick start

```sh
nulang nula new tasks --template web --style syntyr
cd tasks
nulang nula dev
```

Open `http://127.0.0.1:8787/`. The app renders server-side, uses POST forms
that continue working without JavaScript, and can use Nulang's existing client
progressive enhancement when it is available. The Todo example stores state in
ephemeral in-memory KV; it is not a production persistence example.

Without `--style syntyr`, the existing web template is unchanged. You can
use other CSS libraries or plain HTML and CSS without any change to Nulang.

## Source, licensing, and offline determinism

The generated project includes:

| Generated path | Role |
| --- | --- |
| `src/main.nula` | Typed HTML, routes and server-side Todo actions |
| `public/syntyr.core.css` | Pinned Syntyr component/tokens core |
| `public/syntyr.semantic.css` | Optional scoped native-HTML styling |
| `public/style.css` | Application-owned layout overrides |
| `SYNTYR-LICENSE` | Upstream MIT license |
| `README.md` | Generated app's run and customization instructions |

Core and semantic CSS files are byte-for-byte vendored from Syntyr's
`site-releases/0.3.0-alpha.4/` distribution. Asset versions are pinned
at Nulang build time using `include_str!`; generating a project never calls
npm, downloads a remote stylesheet, or executes JavaScript.
The CSS assets are not loaded by apps that do not opt in. Any upgrade should
copy both release files, keep the license, and run the scaffold regression tests.

## Design boundaries

- CSS classes and design tokens are presentation only. Use standard form
  actions, Nulang routes, and correct ARIA state for interactions.
- `.syn-root.syn-semantic` scopes semantic styling to the application root;
  don't change global HTML semantics or inject CSS into the web runtime.
- `data-syn-theme="studio"` is a neutral SaaS default; other themes are
  optional and should not be forced onto every Nulang project.
- Keep the Syntyr version pinned. Its alpha stylesheet API is not a frozen
  compatibility contract; users can update the vendored assets deliberately.
- Prefer standard HTML and browser-native functionality to adding a dedicated
  Syntyr/Nulang component runtime or second state-management layer.
- The preset is deliberately removable: strip the Syntyr links and classes,
  then style the existing native HTML with any CSS system.

Upstream Syntyr: https://github.com/dporkka/syntyr-css
