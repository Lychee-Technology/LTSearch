"""Release 自动化结构守卫（#113）。

锁定四件事：
1. release.yml 的 tag 触发发布结构（打包 → GHCR 单一 local 镜像 → GitHub Release）；
2. scripts/package-release.sh 的组装契约（复用既有打包脚本，产出 checksums+provenance）；
3. ci.yml 以 stub 模式校验组装路径而不发布（release-assembly job）；
4. 已退役发布面（server 镜像栈、image-based Lambda）的墓碑——防止回潮。
"""

import stat
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "release.yml"
CI_WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"
PACKAGE_RELEASE_PATH = REPO_ROOT / "scripts" / "package-release.sh"


class ReleaseWorkflowTest(unittest.TestCase):
    def test_release_workflow_matches_approved_structure(self) -> None:
        self.assertTrue(
            RELEASE_WORKFLOW_PATH.exists(), f"missing workflow: {RELEASE_WORKFLOW_PATH}"
        )

        lines = RELEASE_WORKFLOW_PATH.read_text(encoding="utf-8").splitlines()
        self.assertEqual(lines[0], "name: Release")

        content = "\n".join(lines)
        self.assertIn("FORCE_JAVASCRIPT_ACTIONS_TO_NODE24: true", content)
        # 仅 tag 触发：main push 只跑 CI（#113 裁决 3）。
        self.assertIn('tags: ["v*"]', content)
        self.assertNotIn("branches:", content)
        self.assertNotIn("schedule:", content)
        # gh release create 需要 contents: write，GHCR push 需要 packages: write。
        self.assertIn("contents: write", content)
        self.assertIn("packages: write", content)
        self.assertIn("runs-on: ubuntu-24.04-arm", content)
        self.assertIn("timeout-minutes: 120", content)
        self.assertIn("uses: actions/checkout@v6", content)

        # LTEmbed 锁定 rev 暂存（与 Cargo.lock 一致，沿袭原 publish-images.yml）。
        self.assertIn("Cargo.lock", content)
        self.assertIn("LTEmbed?branch=main#", content)
        self.assertIn(".sam-local-deps/LTEmbed", content)

        # 发布产物统一由 package-release.sh 组装（real 模式）。
        self.assertIn("bash scripts/package-release.sh --mode real", content)

        # 恰一个 local OCI 镜像（AC-a）：arm64 构建 + 架构断言（#130 口径），无 buildx。
        self.assertIn("-f sam/local.Dockerfile", content)
        self.assertIn("ghcr.io/lychee-technology/ltsearch-local", content)
        self.assertIn("docker inspect --format '{{.Architecture}}'", content)
        self.assertNotIn("buildx", content)

        # GHCR 登录用内置 token；latest 仅限稳定语义版本。
        self.assertIn("secrets.GITHUB_TOKEN", content)
        self.assertIn("docker login ghcr.io", content)
        self.assertIn("^v[0-9]+\\.[0-9]+\\.[0-9]+$", content)

        # GitHub Release 用预装 gh CLI，资产 = dist/release 全套。
        self.assertIn("gh release create", content)
        self.assertIn("--verify-tag", content)
        self.assertIn("--prerelease", content)
        self.assertIn("dist/release/", content)

        # 退役面不得回潮：server 镜像、sha- tag 谱系。
        self.assertNotIn("_server", content)
        self.assertNotIn("sha-", content)

    def test_package_release_script_assembles_full_payload(self) -> None:
        self.assertTrue(
            PACKAGE_RELEASE_PATH.exists(), f"missing script: {PACKAGE_RELEASE_PATH}"
        )
        mode = PACKAGE_RELEASE_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "package-release.sh must be executable")

        content = PACKAGE_RELEASE_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        # 复用既有打包脚本，不重复构建逻辑；发布前过尺寸/架构闸门。
        self.assertIn("package-lambda-zips.sh", content)
        self.assertIn("package-model-assets.sh", content)
        self.assertIn("check-lambda-size-budget.sh", content)
        # 全套产物：3 函数 zip + model-assets.zip + provenance + checksums。
        self.assertIn("model-assets.zip", content)
        self.assertIn("release-provenance.json", content)
        self.assertIn("SHA256SUMS", content)
        self.assertIn("schema_version", content)
        # 不引入第三方打包工具（#109 裁决沿用）。
        self.assertNotIn("cargo-lambda", content)

    def test_ci_validates_release_assembly_without_publishing(self) -> None:
        content = CI_WORKFLOW_PATH.read_text(encoding="utf-8")
        lines = content.splitlines()
        jobs = _parse_jobs(lines)
        self.assertIn("release-assembly", jobs)

        release_assembly = jobs["release-assembly"]
        # stub 模式组装 + checksum 回验；绝不触碰发布面。
        self.assertIn("bash scripts/package-release.sh --mode stub", release_assembly)
        self.assertIn("sha256sum -c SHA256SUMS", release_assembly)
        self.assertNotIn("ghcr", release_assembly)
        self.assertNotIn("gh release", release_assembly)

    def test_retired_publishing_surfaces_are_gone(self) -> None:
        # 组件镜像发布与 image-based Lambda 的墓碑（#113 裁决 1/2 及派生删除）。
        retired = [
            ".github/workflows/publish-images.yml",
            "template.sam-e2e.yaml",
            "docker-compose.http.yml",
            "Dockerfile",
            "sam/query_server.Dockerfile",
            "sam/write_server.Dockerfile",
            "sam/index_builder_server.Dockerfile",
            "sam/query_lambda.Dockerfile",
            "sam/write_lambda.Dockerfile",
            "sam/index_builder_lambda.Dockerfile",
            "src/bin/query_server.rs",
            "src/bin/write_server.rs",
            "src/bin/index_builder_server.rs",
            "scripts/e2e/run-http-server-flow.sh",
            "scripts/e2e/run-sam-local-invoke-e2e.sh",
            "scripts/e2e/start-sam-moto.sh",
            "scripts/e2e/stop-sam-moto.sh",
            "scripts/e2e/run-http-flow.sh",
            "tests/test_sam_invoke_e2e.py",
            "tests/test_sam_start_api_e2e.py",
        ]
        for rel in retired:
            self.assertFalse(
                (REPO_ROOT / rel).exists(), f"retired surface resurfaced: {rel}"
            )

        cargo_toml = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        for bin_name in ("query_server", "write_server", "index_builder_server"):
            self.assertNotIn(bin_name, cargo_toml)


def _parse_jobs(lines: list[str]) -> dict[str, str]:
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
