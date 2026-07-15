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
        self.assertEqual(
            set(jobs.keys()),
            {"fast", "feature-matrix", "integration", "sam-e2e", "http-e2e"},
        )

        fast = jobs["fast"]
        self.assertIn("runs-on: ubuntu-24.04-arm", fast)
        self.assertIn("timeout-minutes: 30", fast)
        self.assertIn("uses: actions/checkout@v6", fast)
        self.assertIn(
            "ref: ${{ github.event_name == 'pull_request' && github.event.pull_request.head.sha || github.sha }}",
            fast,
        )
        self.assertIn("uses: actions/setup-python@v6", fast)
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@v1", fast)
        self.assertIn("cache: true", fast)
        self.assertIn("run: python3 -B tests/test_ci_workflow.py", fast)
        self.assertIn("run: python3 -B tests/test_readme_workflow.py", fast)
        self.assertIn("run: bash scripts/verify-fast.sh", fast)
        self.assertNotIn("docker compose -f docker-compose.moto.yml up -d", fast)

        feature_matrix = jobs["feature-matrix"]
        self.assertIn("runs-on: ubuntu-24.04-arm", feature_matrix)
        self.assertIn("timeout-minutes: 45", feature_matrix)
        self.assertIn("uses: actions/checkout@v4", feature_matrix)
        self.assertIn(
            "uses: actions-rust-lang/setup-rust-toolchain@v1", feature_matrix
        )
        # The local profile must build AWS-free and prove no AWS/Lambda crate
        # leaked into its dependency graph.
        self.assertIn(
            "cargo build --no-default-features --features local", feature_matrix
        )
        for pkg in ("aws-config", "aws-sdk-s3", "aws-sdk-sqs", "lambda_runtime"):
            self.assertIn(pkg, feature_matrix)
        self.assertIn(
            'cargo tree --no-default-features --features local -i "$pkg"',
            feature_matrix,
        )
        self.assertIn("leaked into the local build graph", feature_matrix)
        self.assertIn("::error::", feature_matrix)
        self.assertIn(
            "cargo test --no-default-features --features local --lib --tests",
            feature_matrix,
        )
        self.assertIn(
            "cargo build --no-default-features --features aws", feature_matrix
        )
        self.assertIn(
            "cargo test --no-default-features --features aws --lib", feature_matrix
        )
        # the aws construction proof (runtime_aws_test) must run continuously,
        # not just the lib unit tests
        self.assertIn("--test runtime_aws_test", feature_matrix)
        self.assertIn(
            "cargo build --no-default-features --features lambda "
            "--bin query_lambda --bin write_lambda --bin index_builder_lambda",
            feature_matrix,
        )

        integration = jobs["integration"]
        self.assertIn("runs-on: ubuntu-24.04-arm", integration)
        self.assertIn("timeout-minutes: 30", integration)
        self.assertIn("uses: actions/checkout@v6", integration)
        self.assertNotIn("github.event.pull_request.head.sha", integration)
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@v1", integration)
        self.assertIn("cache: true", integration)
        self.assertIn("run: bash scripts/verify-moto.sh", integration)
        self.assertIn(
            "if: always()\n        run: docker compose -f docker-compose.moto.yml down -v",
            integration,
        )
        self.assertNotIn("run: cargo fmt --check", integration)
        self.assertNotIn(
            "run: cargo clippy --all-targets --all-features -- -D warnings", integration
        )

        sam_e2e = jobs["sam-e2e"]
        self.assertIn("needs: integration", sam_e2e)
        self.assertIn("runs-on: ubuntu-24.04-arm", sam_e2e)
        self.assertIn("timeout-minutes: 120", sam_e2e)
        self.assertIn("uses: actions/checkout@v6", sam_e2e)
        self.assertIn("uses: actions/setup-python@v6", sam_e2e)
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@v1", sam_e2e)
        self.assertIn("run: python3 -B tests/test_sam_invoke_e2e.py", sam_e2e)
        self.assertIn(
            "run: python3 -m pip install --upgrade pip awscli aws-sam-cli", sam_e2e
        )
        self.assertIn("run: docker compose -f docker-compose.moto.yml up -d", sam_e2e)
        self.assertIn("run: bash scripts/e2e/run-sam-local-invoke-e2e.sh", sam_e2e)
        self.assertIn(
            "if: always()\n        run: docker compose -f docker-compose.moto.yml down -v",
            sam_e2e,
        )

        http_e2e = jobs["http-e2e"]
        self.assertIn("needs: integration", http_e2e)
        self.assertIn("runs-on: ubuntu-24.04-arm", http_e2e)
        self.assertIn("timeout-minutes: 120", http_e2e)
        self.assertIn("uses: actions/checkout@v6", http_e2e)
        self.assertIn("uses: actions/setup-python@v6", http_e2e)
        self.assertIn(
            "docker build -f sam/builder.Dockerfile -t ltsearch-e2e-builder --build-arg LTEMBED_MODE=stub .",
            http_e2e,
        )
        self.assertIn(
            "docker compose -f docker-compose.http.yml up -d --wait", http_e2e
        )
        self.assertIn("bash scripts/e2e/run-http-server-flow.sh", http_e2e)
        self.assertIn(
            "if: always()\n        run: docker compose -f docker-compose.http.yml down -v",
            http_e2e,
        )

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

            if (
                line.startswith("  ")
                and line.endswith(":")
                and not line.startswith("    ")
            ):
                current_job = line.strip()[:-1]
                jobs[current_job] = []
                continue

            if current_job is not None:
                jobs[current_job].append(line)

        return {name: "\n".join(block) for name, block in jobs.items()}


if __name__ == "__main__":
    unittest.main()
