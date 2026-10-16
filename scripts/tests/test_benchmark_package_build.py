import importlib.util
import pathlib
import tempfile
import textwrap
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "benchmark_package_build.py"
spec = importlib.util.spec_from_file_location("benchmark_package_build", SCRIPT)
benchmark = importlib.util.module_from_spec(spec)
if SCRIPT.is_file():
    spec.loader.exec_module(benchmark)


class PackageBuildBenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.package = self.root / "package"
        (self.package / "src").mkdir(parents=True)
        (self.package / "Nulang.toml").write_text(
            '[package]\nname = "benchprobe"\nversion = "0.1.0"\n'
        )
        (self.package / "src" / "main.nula").write_text("fn main() = 42\n")

    def fake_compiler(self, name: str, *, output: str = "stable", fail=False):
        compiler = self.root / name
        compiler.write_text(textwrap.dedent(f'''\
            #!/usr/bin/env python3
            import json
            import pathlib
            import sys
            path = pathlib.Path.cwd() / ".nula" / "dist"
            path.mkdir(parents=True, exist_ok=True)
            (path / "benchprobe.nbc").write_bytes({output!r}.encode())
            (path / "benchprobe.behavior.json").write_text('{{"v":1}}')
            if "--json" in sys.argv:
                print(json.dumps({{"schema_version": 1, "command": "build", "ok": {str(not fail)}, "diagnostics": []}}))
            if {str(fail)}:
                sys.exit(1)
            '''))
        compiler.chmod(0o755)
        return compiler

    def test_alternates_order_between_rounds(self):
        self.assertEqual(("base", "candidate"), benchmark.alternating_order(0))
        self.assertEqual(("candidate", "base"), benchmark.alternating_order(1))

    def test_reports_paired_fresh_and_repeat_samples_without_real_compiler(self):
        base = self.fake_compiler("base.py")
        candidate = self.fake_compiler("candidate.py")
        report = benchmark.benchmark_packages(base, candidate, self.package, rounds=2, modes=("normal", "json"))
        self.assertEqual(2, report["rounds"])
        for mode in ("normal", "json"):
            for phase in ("fresh", "repeat"):
                self.assertEqual(2, len(report["samples_ns"][mode][phase]["base"]))
                self.assertEqual(2, len(report["samples_ns"][mode][phase]["candidate"]))
                self.assertGreater(report["paired_speedup_x"][mode][phase], 0)

    def test_fails_closed_on_artifact_mismatch(self):
        base = self.fake_compiler("base.py", output="A")
        candidate = self.fake_compiler("candidate.py", output="B")
        with self.assertRaisesRegex(benchmark.BenchmarkError, "artifact mismatch"):
            benchmark.benchmark_packages(base, candidate, self.package, rounds=1, modes=("normal",))

    def test_fails_closed_on_compiler_error(self):
        base = self.fake_compiler("base.py", fail=True)
        candidate = self.fake_compiler("candidate.py")
        with self.assertRaisesRegex(benchmark.BenchmarkError, "compiler exited"):
            benchmark.benchmark_packages(base, candidate, self.package, rounds=1, modes=("normal",))

    def test_requires_nonempty_source_package(self):
        base = self.fake_compiler("base.py")
        with self.assertRaisesRegex(benchmark.BenchmarkError, "Nulang.toml"):
            benchmark.benchmark_packages(base, base, self.root, rounds=1, modes=("normal",))


if __name__ == "__main__":
    unittest.main()
