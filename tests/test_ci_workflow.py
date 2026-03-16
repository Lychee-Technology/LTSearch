import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"


class CiWorkflowTest(unittest.TestCase):
    def test_ci_workflow_matches_approved_structure(self) -> None:
        self.assertTrue(WORKFLOW_PATH.exists(), f"missing workflow: {WORKFLOW_PATH}")

        lines = WORKFLOW_PATH.read_text(encoding="utf-8").splitlines()
        self.assertEqual(lines[0], "name: CI")

        content = "\n".join(lines)
        self.assertIn("FORCE_JAVASCRIPT_ACTIONS_TO_NODE24: true", content)
        self.assertIn("pull_request:", content)
        self.assertIn("workflow_dispatch:", content)
        self.assertIn("push:\n    branches:\n      - main", content)
        self.assertNotIn("schedule:", content)
        self.assertNotIn("deploy", content.lower())

        jobs = self._parse_jobs(lines)
        self.assertEqual(set(jobs.keys()), {"lint", "test"})

        lint = jobs["lint"]
        self.assertIn("runs-on: [self-hosted, Linux, ARM64]", lint)
        self.assertIn("timeout-minutes: 20", lint)
        self.assertIn("uses: actions/checkout@v6", lint)
        self.assertIn(
            "ref: ${{ github.event_name == 'pull_request' && github.event.pull_request.head.sha || github.sha }}",
            lint,
        )
        self.assertIn("uses: actions/setup-python@v6", lint)
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@v1", lint)
        self.assertIn("cache: true", lint)
        self.assertIn("run: python3 -B tests/test_ci_workflow.py", lint)
        self.assertIn("run: cargo fmt --check", lint)
        self.assertIn("run: cargo clippy --all-targets --all-features -- -D warnings", lint)
        self.assertNotIn("run: cargo test", lint)

        test = jobs["test"]
        self.assertIn("runs-on: [self-hosted, Linux, ARM64]", test)
        self.assertIn("timeout-minutes: 30", test)
        self.assertIn("uses: actions/checkout@v6", test)
        self.assertNotIn("github.event.pull_request.head.sha", test)
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@v1", test)
        self.assertIn("cache: true", test)
        self.assertIn("run: docker compose -f docker-compose.localstack.yml up -d", test)
        self.assertIn("run: cargo test", test)
        self.assertIn(
            "if: always()\n        run: docker compose -f docker-compose.localstack.yml down -v",
            test,
        )
        self.assertNotIn("run: cargo fmt --check", test)
        self.assertNotIn("run: cargo clippy --all-targets --all-features -- -D warnings", test)

    def _parse_jobs(self, lines: list[str]) -> dict[str, str]:
        jobs: dict[str, list[str]] = {}
        in_jobs = False
        current_job: str | None = None

        for line in lines:
            if line == "jobs:":
                in_jobs = True
                continue

            if not in_jobs:
                continue

            if line and not line.startswith(" "):
                break

            if line.startswith("  ") and line.endswith(":") and not line.startswith("    "):
                current_job = line.strip()[:-1]
                jobs[current_job] = []
                continue

            if current_job is not None:
                jobs[current_job].append(line)

        return {name: "\n".join(block) for name, block in jobs.items()}


if __name__ == "__main__":
    unittest.main()
