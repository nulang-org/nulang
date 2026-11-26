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
Nulang Web and this demo are experimental. The development server listens on
all interfaces: use an isolated trusted network, do not expose the port to the
Internet, and do not enter sensitive data. Authentication, CSRF protection and
durable persistence are not demonstrated by this sample.

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
| `SYNTYR-LICENSE` and `public/SYNTYR-LICENSE.txt` | Upstream MIT license; static build carries the public copy alongside CSS |
| `README.md` | Generated app's run and customization instructions |

Core and semantic CSS files are byte-for-byte vendored from Syntyr's
`site-releases/0.3.0-alpha.4/` distribution. Asset versions are pinned
at Nulang build time using `include_str!`; generating a project never calls
npm, downloads a remote stylesheet, or executes JavaScript.
The CSS assets are not loaded by apps that do not opt in. Both CSS files and
the license match the published tag `v0.3.0-alpha.4` (commit
`fd95f24fef8698637d43fdff0012e5c12a0a2783`) byte for byte.

```text
8d6458bf1f2ae25098a733e7fb88a84260e54b8b285bf05ff07432cccfbdaa6e  public/syntyr.core.css
0d710710c605f141f47a695cbc5a7e7e89b7556ddd7ea7df48453b6b0d3055f0  public/syntyr.semantic.css
42f56369ac06362dbfe433d2d50b178bf74c369a55ea22337ab40fd9433521bf  public/SYNTYR-LICENSE.txt
```

Use `sha256sum public/syntyr.core.css public/syntyr.semantic.css public/SYNTYR-LICENSE.txt`
to verify the installed assets. Any upgrade should copy both release files,
keep the license, update the pin and hashes and run the scaffold/build regression
tests. A static build copies the public notice with the assets.

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
