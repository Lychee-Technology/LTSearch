import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "run-static-release-flow.sh"
EXAMPLE_PATH = REPO_ROOT / "examples" / "emit_static_lance_fixture.rs"


class StaticReleaseE2ETest(unittest.TestCase):
    """Structure guard for the static release v3 end-to-end flow (Task 14).

    Mirrors the intent of test_e2e_fixtures.py: assert the driver script and its
    Lance fixture example exist and encode the required five-step sequence and
    assertion points, without running the (slow, cargo-heavy) flow here.
    """

    def test_static_release_example_exists(self) -> None:
        self.assertTrue(
            EXAMPLE_PATH.exists(),
            f"missing static Lance fixture example: {EXAMPLE_PATH}",
        )
        example = EXAMPLE_PATH.read_text(encoding="utf-8")
        # Emits a 512-dim `documents` table with citable metadata + variant swap.
        self.assertIn("documents", example)
        self.assertIn("--variant", example)
        self.assertIn("resource_id", example)
        self.assertIn("\"lang\"", example)

    def test_static_release_script_exists(self) -> None:
        self.assertTrue(
            SCRIPT_PATH.exists(),
            f"missing static release E2E script: {SCRIPT_PATH}",
        )

    def test_static_release_script_has_expected_shape(self) -> None:
        content = SCRIPT_PATH.read_text(encoding="utf-8")

        self.assertIn("set -euo pipefail", content)

        # The three static subcommands must appear in build -> activate -> query
        # order, after the dynamic write -> build prerequisite.
        build_at = content.find('"$BIN" static-build')
        activate_at = content.find('"$BIN" static-activate')
        query_at = content.find("$BASE_QUERY/query")
        self.assertNotEqual(build_at, -1, "script must invoke static-build")
        self.assertNotEqual(activate_at, -1, "script must invoke static-activate")
        self.assertNotEqual(query_at, -1, "script must POST /query")
        self.assertLess(
            build_at, activate_at, "static-build must precede static-activate"
        )
        self.assertLess(
            activate_at, query_at, "static-activate must precede /query"
        )

        # Assertion points: response static_release_id + citation fields.
        self.assertIn("static_release_id", content)
        self.assertIn("citation", content)
        self.assertIn("resource_id", content)
        self.assertIn("static_chunks", content)

        # Second release must flip the /health static_release_id pointer.
        self.assertIn("/health", content)
        self.assertIn("--variant", content)


if __name__ == "__main__":
    unittest.main()
