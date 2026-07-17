import stat
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
ASSETS_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-model-assets.sh"
ZIPS_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-lambda-zips.sh"
BUDGET_SCRIPT_PATH = REPO_ROOT / "scripts" / "check-lambda-size-budget.sh"
BUILDER_DOCKERFILE_PATH = REPO_ROOT / "sam" / "builder.Dockerfile"


class ModelAssetsPackagingTest(unittest.TestCase):
    def test_assets_script_builds_bundle_stage_and_writes_manifest(self) -> None:
        self.assertTrue(ASSETS_SCRIPT_PATH.exists())
        content = ASSETS_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("sam/builder.Dockerfile", content)
        self.assertIn("--platform linux/arm64", content)
        self.assertIn("--target bundle", content)
        self.assertIn("LTEMBED_MODE=real", content)
        # 输出 dist/model-assets/ 平铺文件 + manifest.json(逐文件 sha256),
        # 供部署前 aws s3 cp --recursive 一次上传。
        self.assertIn("model-assets", content)
        self.assertIn("manifest.json", content)
        self.assertIn("sha256", content)
        self.assertNotIn("cargo-lambda", content)
        mode = ASSETS_SCRIPT_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "assets script must be executable")

    def test_zip_script_strips_bootstrap_in_builder_image(self) -> None:
        content = ZIPS_SCRIPT_PATH.read_text(encoding="utf-8")
        # strip 必须在 AL2023 builder 镜像内做(宿主机无 aarch64 binutils);
        # real 二进制 235.7→180.6 MiB,不 strip 则逼近 250MB 单函数硬限。
        self.assertIn("strip", content)
        self.assertIn("docker run", content)

    def test_budget_script_asserts_250mb_arch_and_asset_hashes(self) -> None:
        self.assertTrue(BUDGET_SCRIPT_PATH.exists())
        content = BUDGET_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        # Lambda 单函数解压硬限 250MB(262,144,000 bytes)。
        self.assertIn("250", content)
        # ELF e_machine == 0xB7(AArch64),不依赖宿主机 file 命令。
        self.assertIn("0xB7", content)
        self.assertIn("e_machine", content)
        self.assertIn("bootstrap", content)
        self.assertIn("libonnxruntime.so", content)
        self.assertIn("model-assets", content)
        self.assertIn("manifest.json", content)
        for fn in ("query_lambda", "write_lambda", "index_builder_lambda"):
            self.assertIn(fn, content)
        mode = BUDGET_SCRIPT_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "budget script must be executable")

    def test_builder_dockerfile_pins_bundle_sha256_in_bundle_stage(self) -> None:
        content = BUILDER_DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertIn("AS bundle", content)
        self.assertIn("ARG LTEMBED_BUNDLE_SHA256=", content)
        self.assertIn("sha256sum -c", content)


if __name__ == "__main__":
    unittest.main()
