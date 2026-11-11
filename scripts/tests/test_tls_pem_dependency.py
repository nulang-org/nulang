"""Prevent reintroduction of the abandoned rustls-pemfile parsing dependency."""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]


class TlsPemDependencyTests(unittest.TestCase):
    def test_tls_loader_uses_maintained_rustls_pki_types_pem_parser(self):
        manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        loader = (ROOT / "src/runtime/network.rs").read_text(encoding="utf-8")
        self.assertFalse(
            any(line.lstrip().startswith("rustls-pemfile =") for line in manifest.splitlines()),
            "the unmaintained parser must not return to Cargo.toml",
        )
        self.assertNotIn("rustls_pemfile::", loader, "use the maintained PEM parser")
        self.assertIn("rustls::pki_types::pem::PemObject", loader)


if __name__ == "__main__":
    unittest.main()
