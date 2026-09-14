# Mobile build artifact contract

Status: Experimental foundation.

Nulang native applications consume the ordinary frozen `.nbc` artifact. The mobile layer does not define a second bytecode format, compiler backend, or VM.

`src/mobile` defines `MobileBuildManifest`, the machine-readable host contract that future `nula build --mobile` packaging writes alongside a validated `app.nbc`.

## Intended output

The packaging command will produce:

```text
.nula/dist/mobile/
├── app.nbc
└── nulang-mobile.json
```

`app.nbc` remains a canonical Nulang bytecode artifact and must pass `CodeModule::from_nbc` before packaging. The manifest records enough information for a host or release pipeline to reject an unexpected artifact before passing it to `nulang_load_nbc`.

## Manifest contract

Version 1 records:

- package and artifact names;
- artifact byte length and BLAKE3 digest;
- canonical `.nbc` format and Nulang language versions;
- C embedding ABI identifier (`nulang-embed/1`);
- semantic UI protocol (`nulang-ui/1`);
- runtime message protocol (`nulang-ui-msg/1`);
- native callback symbols (`nulang_ui_document`, `nulang_ui_message`);
- the current coarse callback capability (`os`);
- declared package capabilities;
- per-platform interpreter constructor and library kind.

Both iOS and Android are intentionally declared as interpreter execution in v1. iOS consumes the root static library through `NulangMobile.xcframework`; Android consumes the dynamic-library form built from the same interpreter runtime.

## Integrity

`MobileBuildManifest::verify_artifact` checks both byte length and BLAKE3. This is an integrity/packaging check, not a replacement for `.nbc` format validation: the runtime must still decode the artifact through the canonical versioned decoder.

## Current implementation boundary

This change establishes the public manifest model and integrity contract only. It does **not** yet add `nula build --mobile` to the package CLI. Keeping those changes separate avoids coupling a small durable host contract to a large package-command rewrite.

The next packaging slice should reuse the ordinary package build, decode the produced `.nbc`, copy it to the stable `mobile/app.nbc` path, serialize this manifest, and verify the resulting pair in integration tests.

Client-action metadata will extend this manifest only after the compiler-authorized `nulang-action-invoke/1` / `nulang-action-result/1` ABI is implemented. It must not be represented as arbitrary exported-function invocation.
