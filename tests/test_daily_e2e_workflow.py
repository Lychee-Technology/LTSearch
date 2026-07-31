import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "e2e-local-real.yml"
CI_WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"
README_PATH = REPO_ROOT / "README.md"
DEPLOYMENT_DOC_PATH = REPO_ROOT / "docs" / "deployment.md"


class DailyE2eWorkflowTest(unittest.TestCase):
    """#144: real-LTEmbed local HTTP 套件的每日/手动 arm64 回归 workflow。

    重型套件不进 PR gate（ci.yml 结构守卫另行禁止 schedule），所以放在
    独立 workflow；这里锁定其触发面、runner、无条件构建、runner 顺序、
    失败诊断与清理契约。
    """

    def test_workflow_matches_approved_structure(self) -> None:
        self.assertTrue(WORKFLOW_PATH.exists(), f"missing workflow: {WORKFLOW_PATH}")

        lines = WORKFLOW_PATH.read_text(encoding="utf-8").splitlines()
        self.assertEqual(lines[0], "name: Local Real LTEmbed E2E")
        content = "\n".join(lines)

        # 触发面：仅每日 schedule + 手动 dispatch；绝不作为 PR 必经检查。
        self.assertIn("schedule:", content)
        self.assertIn("cron:", content)
        self.assertIn("workflow_dispatch:", content)
        self.assertNotIn("pull_request", content)
        self.assertNotIn("push:", content)

        # 已在跑的每日回归不被后续触发取消。
        self.assertIn("cancel-in-progress: false", content)

        jobs = self._parse_jobs(lines)
        self.assertEqual(set(jobs.keys()), {"real-ltembed-e2e"})
        job = jobs["real-ltembed-e2e"]

        self.assertIn("runs-on: ubuntu-24.04-arm", job)
        self.assertIn("timeout-minutes: 120", job)
        self.assertIn("uses: actions/checkout@v6", job)
        self.assertIn("uses: actions/setup-python@v6", job)

        # 镜像必须无条件从当前 checkout 构建（PR #151 review 硬要求）：
        # runner 的「tag 存在即跳过构建」不得成为 CI 路径——显式构建步骤
        # 必须先于全部 runner 执行。
        self.assertIn("run: bash scripts/e2e/build-local-ltembed-image.sh", job)

        runners = [
            "run: bash scripts/e2e/run-local-real-flow.sh",
            "run: bash scripts/e2e/run-local-real-degraded-health.sh",
            "run: bash scripts/e2e/run-local-real-dynamic-contract.sh",
            "run: bash scripts/e2e/run-local-real-static-contract.sh",
        ]
        positions = []
        for runner in runners:
            self.assertIn(runner, job, f"missing runner step: {runner}")
            positions.append(job.index(runner))
        self.assertEqual(positions, sorted(positions), "runners must run in order")
        self.assertLess(
            job.index("build-local-ltembed-image.sh"),
            positions[0],
            "image must be built from checkout before any runner",
        )

        # 失败诊断：lhttp 失败路径把服务日志与 HTTP 载荷保留在 .e2e-tmp/
        # 下；成功路径会清空 run 目录，所以上传必须容忍目录不存在。
        self.assertIn("if: failure()", job)
        self.assertIn("uses: actions/upload-artifact@v4", job)
        self.assertIn("path: .e2e-tmp/", job)
        self.assertIn("if-no-files-found: ignore", job)
        self.assertIn("retention-days: 14", job)

        # teardown 兜底：无论成败清理泄漏的 per-run compose 资源（四个
        # runner 的 project 前缀都是 ltsearch-real）。
        self.assertIn("if: always()", job)
        self.assertIn('--filter "name=ltsearch-real"', job)
        for resource in ("docker rm -f", "docker volume rm -f", "docker network rm"):
            self.assertIn(resource, job)

    def test_guard_is_registered_in_ci_fast_job(self) -> None:
        # 本守卫必须由 PR gate 运行，否则对 workflow 的改动不会被拦截。
        content = CI_WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("run: python3 -B tests/test_daily_e2e_workflow.py", content)

    def test_docs_describe_daily_regression(self) -> None:
        readme = README_PATH.read_text(encoding="utf-8")
        # README 不再把每日回归指给 issue 编号，而是指向落地的 workflow，
        # 并说明手动触发、模型下载/耗时、arm64 限制与诊断产物。
        self.assertNotIn("daily CI regression is tracked by #144", readme)
        self.assertIn("Local Real LTEmbed E2E", readme)
        self.assertIn("e2e-local-real.yml", readme)
        self.assertIn("gh workflow run e2e-local-real.yml", readme)
        self.assertIn("Linux/arm64", readme)

        deployment = DEPLOYMENT_DOC_PATH.read_text(encoding="utf-8")
        self.assertNotIn("每日 CI 回归归 #144", deployment)
        self.assertIn("e2e-local-real.yml", deployment)

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
