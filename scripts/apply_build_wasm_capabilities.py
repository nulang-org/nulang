#!/usr/bin/env python3
from pathlib import Path

p = Path('src/package/commands.rs')
s = p.read_text()

anchor = '''/// `nula build-wasm`: compile package to .wasm + AOT .cwasm.\n/// `nula build-wasm`: compile package to .wasm + AOT .cwasm in .nula/dist/.\nfn cmd_build_wasm() -> NuResult<()> {'''
if s.count(anchor) != 1:
    raise SystemExit(f'cmd_build_wasm anchor count={s.count(anchor)}')
helper = '''/// Build compiler arguments for the canonical WASM AOT package path.\n///\n/// Keep package capability forwarding centralized here so `build-wasm` has\n/// the same default-deny semantics as `build`/`run`.\nfn wasm_aot_args(wasm_path: &str, entry: &str) -> Vec<String> {\n    let mut args = vec![\n        "--backend".to_string(),\n        "wasm-aot".to_string(),\n        "--out".to_string(),\n        wasm_path.to_string(),\n        entry.to_string(),\n    ];\n    args.extend(capability_args());\n    args\n}\n\n'''
s = s.replace(anchor, helper + anchor, 1)

old = '''    nulang_exe(&["--backend", "wasm-aot", "--out", &wasm_path_str, &entry_str])?;'''
new = '''    let args = wasm_aot_args(&wasm_path_str, &entry_str);\n    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();\n    nulang_exe(&arg_refs)?;'''
if s.count(old) != 1:
    raise SystemExit(f'build-wasm invocation count={s.count(old)}')
s = s.replace(old, new, 1)

# Add a focused pure argument-construction regression at the beginning of the
# existing tests module. No Wasmtime process is required for this invariant.
test_anchor = '''mod tests {\n    use super::*;\n'''
if s.count(test_anchor) != 1:
    raise SystemExit(f'tests anchor count={s.count(test_anchor)}')
test = r'''

    #[test]
    fn test_build_wasm_forwards_manifest_capabilities() {
        let dir = std::env::temp_dir().join(format!(
            "nulang_build_wasm_caps_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            "[package]\nname = \"caps-test\"\nversion = \"0.1.0\"\ncapabilities = [\"net\", \"fs\"]\n\n[dependencies]\n",
        )
        .unwrap();
        let _guard = ChangeDir::new(&dir);

        let args = wasm_aot_args("/tmp/out.wasm", "/tmp/main.nula");
        assert_eq!(
            args,
            vec![
                "--backend",
                "wasm-aot",
                "--out",
                "/tmp/out.wasm",
                "/tmp/main.nula",
                "--with",
                "net",
                "--with",
                "fs",
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
'''
s = s.replace(test_anchor, test_anchor + test, 1)
p.write_text(s)
