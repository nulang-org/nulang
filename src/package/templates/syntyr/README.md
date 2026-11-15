# Nulang Web + Syntyr

This is an optional, self-hosted Syntyr styling preset for the Nulang web
starter. Nulang itself does not require a styling framework or any browser
JavaScript library.

## Run

```sh
nulang nula dev
# open http://127.0.0.1:8787/
```

The initial page is server-rendered; standard HTML forms submit without
JavaScript. Nulang may progressively enhance navigation and form actions if
its generated client runtime is enabled. Todos are stored in ephemeral runtime
memory, so do not treat this example as persistent production storage.

## Styling

- `public/syntyr.core.css` and `public/syntyr.semantic.css` are vendored,
  unmodified Syntyr CSS **0.3.0-alpha.4** release assets (MIT license).
- `SYNTYR-LICENSE` contains the upstream license.
- `public/style.css` contains only application layout and spacing overrides.
- `.syn-root.syn-semantic` scopes the theme; `data-syn-theme="studio"` selects
  the neutral professional palette.
- Modify `public/style.css` or documented `--syn-*` tokens for customization.
  Avoid editing the vendored Syntyr files; update both files and the version
  information together when changing release.
- No npm, CDN, runtime JavaScript dependency, or styling compiler is required.

Upstream: https://github.com/dporkka/syntyr-css

Remove Syntyr by removing its two CSS links and replacing the `syn-*` classes
with your preferred styling; Nulang's routes and server actions remain unchanged.
