import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts" / "nula_verify.py"
spec = importlib.util.spec_from_file_location("nula_verify", MODULE_PATH)
nula_verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nula_verify)


class NulaVerifyProfilesTests(unittest.TestCase):
    def test_profiles_are_stable_and_complete(self):
        self.assertEqual(
            {"fast", "native", "wasm", "durability", "full"},
            set(nula_verify.PROFILES),
        )

    def test_every_profile_starts_with_semantic_registry_validation(self):
        for name in nula_verify.PROFILES:
            with self.subTest(profile=name):
                self.assertEqual(
                    ["python3", "scripts/verify_semantics.py"],
                    nula_verify.profile_commands(name)[0],
                )

    def test_native_profile_exercises_differential_oracle_and_aot_tests(self):
        commands = nula_verify.profile_commands("native")
        flattened = [" ".join(command) for command in commands]
        self.assertTrue(
            any(
                "features native-codegen" in command and "difffuzz" in command
                for command in flattened
            )
        )
        self.assertTrue(
            any(
                "features native-codegen" in command and "aot" in command
                for command in flattened
            )
        )

    def test_wasm_profile_requires_wasm_differential_participation(self):
        commands = nula_verify.profile_commands("wasm")
        flattened = [" ".join(command) for command in commands]
        self.assertTrue(
            any(
                "features wasm-backend" in command and "difffuzz" in command
                for command in flattened
            )
        )

    def test_full_profile_delegates_to_existing_ci_local_contract(self):
        self.assertIn(
            ["bash", "scripts/ci-local.sh"],
            nula_verify.profile_commands("full"),
        )


if __name__ == "__main__":
    unittest.main()
