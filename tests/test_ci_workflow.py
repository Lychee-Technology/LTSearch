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
            {
                "fast",
                "feature-matrix",
                "integration",
                "sam-e2e",
                "sam-zip-e2e",
                "sam-ltembed-e2e",
                "http-e2e",
                "local-image-e2e",
                "local-e2e",
            },
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
        self.assertIn(
            "cargo test --no-default-features --features lambda --bins", feature_matrix
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
            "docker build --platform linux/arm64 -f sam/builder.Dockerfile -t ltsearch-e2e-builder --build-arg LTEMBED_MODE=stub .",
            http_e2e,
        )
        # #130：产物架构守卫——构建后必须 docker inspect 断言 arm64。
        self.assertIn("docker inspect --format '{{.Architecture}}'", http_e2e)
        self.assertIn(
            "docker compose -f docker-compose.http.yml up -d --wait", http_e2e
        )
        self.assertIn("bash scripts/e2e/run-http-server-flow.sh", http_e2e)
        self.assertIn(
            "if: always()\n        run: docker compose -f docker-compose.http.yml down -v",
            http_e2e,
        )

        # 单镜像 SQLite 本地链路（#125）：moto-free、无 awscli/sam，一个镜像三个
        # 角色 + 保留卷重启断言（在 run-local-image-flow.sh 内）。
        local_image_e2e = jobs["local-image-e2e"]
        self.assertIn("needs: integration", local_image_e2e)
        self.assertIn("runs-on: ubuntu-24.04-arm", local_image_e2e)
        self.assertIn("timeout-minutes: 120", local_image_e2e)
        self.assertIn("uses: actions/checkout@v6", local_image_e2e)
        self.assertIn("uses: actions/setup-python@v6", local_image_e2e)
        self.assertIn(
            "docker build --platform linux/arm64 -f sam/local.Dockerfile -t ltsearch-local:dev .",
            local_image_e2e,
        )
        # #130：产物架构守卫——构建后必须 docker inspect 断言 arm64。
        self.assertIn("docker inspect --format '{{.Architecture}}'", local_image_e2e)
        # #125 验收：sam/local.Dockerfile 必须自包含——CI 不得预构建 builder 镜像，
        # 否则会掩盖 Dockerfile 对外部先决镜像的依赖（PR #129 review P1）。
        self.assertNotIn("sam/builder.Dockerfile", local_image_e2e)
        self.assertIn(
            "docker compose -f docker-compose.local.yml up -d --wait", local_image_e2e
        )
        self.assertIn("bash scripts/e2e/run-local-image-flow.sh", local_image_e2e)
        self.assertIn(
            "if: always()\n        run: docker compose -f docker-compose.local.yml down -v",
            local_image_e2e,
        )
        # moto-free：本作业不得触碰 moto compose，也不装 awscli/sam。
        self.assertNotIn("docker-compose.moto.yml", local_image_e2e)
        self.assertNotIn("awscli", local_image_e2e)

        # 原生本地链路（epic #116 AC-2 / #120）：moto-free 且 **Docker-free**——原生
        # 进程跑 write→build→query + 重启耐久性断言，无任何 docker/moto/awscli 依赖。
        # #120 要求 standalone（无 needs:），与 fast/feature-matrix 并行。
        local_e2e = jobs["local-e2e"]
        self.assertNotIn("needs:", local_e2e)
        self.assertIn("runs-on: ubuntu-24.04-arm", local_e2e)
        self.assertIn("timeout-minutes: 45", local_e2e)
        self.assertIn("uses: actions/checkout@v6", local_e2e)
        self.assertIn("uses: actions/setup-python@v6", local_e2e)
        self.assertIn("uses: actions-rust-lang/setup-rust-toolchain@v1", local_e2e)
        self.assertIn("cache: true", local_e2e)
        self.assertIn(
            "run: cargo build --no-default-features --features local --bin ltsearch",
            local_e2e,
        )
        self.assertIn("run: bash scripts/e2e/run-local-server-flow.sh", local_e2e)
        self.assertNotIn("docker", local_e2e)
        self.assertNotIn("awscli", local_e2e)

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
