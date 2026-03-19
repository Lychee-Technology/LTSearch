import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
README_PATH = REPO_ROOT / "README.md"


class ReadmeWorkflowTest(unittest.TestCase):
    def test_readme_documents_fast_and_moto_workflows(self) -> None:
        self.assertTrue(README_PATH.exists(), f"missing README: {README_PATH}")

        content = README_PATH.read_text(encoding="utf-8")

        self.assertIn("## Fast Local Checks", content)
        self.assertIn("bash scripts/verify-fast.sh", content)
        self.assertIn("builds all Lambda binaries", content)
        self.assertIn("runs the non-Moto test suite", content)

        self.assertIn("## Moto-backed Integration Checks", content)
        self.assertIn("bash scripts/verify-moto.sh", content)
        self.assertIn("docker compose -f docker-compose.moto.yml up -d", content)
        self.assertIn("tests/write_build_publish_test.rs", content)

        self.assertIn("## CI", content)
        self.assertIn("fast Docker-free verification path", content)
        self.assertIn("Moto-backed integration path", content)


if __name__ == "__main__":
    unittest.main()
