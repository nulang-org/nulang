import importlib.util
import pathlib
import stat
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
PATH = ROOT / "compare_import_compile.py"
spec = importlib.util.spec_from_file_location("compare_import_compile", PATH)
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)


def sample(mode, iteration, ns=100, modules=2, functions=3):
    import json
    return json.dumps({
        "schema": 1,
        "workload": "transitive_imports",
        "cache": mode,
        "scope": "in_process_import_to_bytecode",
        "iteration": iteration,
        "modules": modules,
        "functions_per_module": functions,
        "resolved_functions": modules * functions,
        "compile_ns": ns,
    })


class ParseTests(unittest.TestCase):
    def test_valid_samples_report_median(self):
        output = "\n".join(sample("enabled", i, ns) for i, ns in enumerate([90, 110, 100], 1))
        self.assertEqual(compare.parse_samples(output, "enabled", 3, 2, 3), 100)

    def test_rejects_wrong_mode_wrong_cardinality_and_bad_duration(self):
        with self.assertRaisesRegex(ValueError, "cache"):
            compare.parse_samples(sample("disabled", 1), "enabled", 1, 2, 3)
        with self.assertRaisesRegex(ValueError, "expected 2 samples"):
            compare.parse_samples(sample("enabled", 1), "enabled", 2, 2, 3)
        with self.assertRaisesRegex(ValueError, "positive integer"):
            compare.parse_samples(sample("enabled", 1, ns=0), "enabled", 1, 2, 3)
        with self.assertRaisesRegex(ValueError, "invalid JSONL"):
            compare.parse_samples("not-json", "enabled", 1, 2, 3)

    def test_paired_order_and_mode_environment(self):
        with tempfile.TemporaryDirectory() as root:
            marker = pathlib.Path(root) / "modes.txt"
            executable = pathlib.Path(root) / "fake-benchmark"
            executable.write_text(
                "#!/usr/bin/env python3\n"
                "import os, json, sys, pathlib\n"
                "args = sys.argv[1:]\n"
                "get = lambda flag: int(args[args.index(flag) + 1])\n"
                "mode = 'disabled' if os.getenv('NULANG_DISABLE_IMPORT_TOKEN_CACHE') == '1' else 'enabled'\n"
                f"with pathlib.Path({str(marker)!r}).open('a') as f: f.write(mode + '\\n')\n"
                "for i in range(1, get('--repeat') + 1):\n"
                " print(json.dumps(dict(schema=1, workload='transitive_imports', cache=mode, scope='in_process_import_to_bytecode', iteration=i, modules=get('--modules'), functions_per_module=get('--functions'), resolved_functions=get('--modules') * get('--functions'), compile_ns=200 if mode == 'disabled' else 100)))\n"
            )
            executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
            report = compare.paired_comparison(executable, 2, 3, 1, 2, 3)
            self.assertEqual(marker.read_text().splitlines(), ["enabled", "disabled", "disabled", "enabled"])
            self.assertEqual(report["median_speedup_disabled_over_enabled"], 2)
            self.assertEqual(len(report["samples"]), 2)


if __name__ == "__main__":
    unittest.main()
