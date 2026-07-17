import stat
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
ASSETS_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-model-assets.sh"
ZIPS_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-lambda-zips.sh"
BUDGET_SCRIPT_PATH = REPO_ROOT / "scripts" / "check-lambda-size-budget.sh"
BUILDER_DOCKERFILE_PATH = REPO_ROOT / "sam" / "builder.Dockerfile"
LTEMBED_E2E_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "run-sam-ltembed-invoke-e2e.sh"
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"


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

    def test_template_provisions_assets_for_query_and_build_only(self) -> None:
        content = (REPO_ROOT / "template.yaml").read_text(encoding="utf-8")
        # S3→/tmp 冷启动供给(#111):无 Layer,资产走 ArtifactBucket 前缀。
        self.assertNotIn("LayerVersion", content)
        self.assertNotIn("Layers", content)
        self.assertIn("ModelAssetPrefix", content)
        for side in ("QUERY", "BUILD"):
            self.assertIn(f"LTSEARCH_{side}_LTEMBED_S3_BUCKET: !Ref ArtifactBucket", content)
            self.assertIn(f"LTSEARCH_{side}_LTEMBED_S3_PREFIX: !Ref ModelAssetPrefix", content)
            self.assertIn(f"LTSEARCH_{side}_LTEMBED_BUNDLE_DIR: /tmp/ltembed", content)
            self.assertIn(
                f"LTSEARCH_{side}_LTEMBED_MODEL_PATH: /tmp/ltembed/model.ort", content
            )
        # write 零模型依赖(AC-5:可独立部署)。
        write_block = content.split("WriteFunction:")[1].split("QueryFunction:")[0]
        self.assertNotIn("LTEMBED", write_block)

    def test_ltembed_e2e_script_covers_real_packaging_budget_and_invoke(self) -> None:
        self.assertTrue(LTEMBED_E2E_SCRIPT_PATH.exists())
        content = LTEMBED_E2E_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("prepare_local_ltembed_checkout", content)
        self.assertIn("LTSEARCH_LTEMBED_MODE=real", content)
        self.assertIn("package-model-assets.sh", content)
        self.assertIn("check-lambda-size-budget.sh", content)
        self.assertIn("assert_zip_layout", content)
        # 资产上传 moto 前缀,函数冷启动 S3→/tmp 下载;不覆盖 provider/dim,
        # 走生产默认 ltembed/512。
        self.assertIn("s3 cp --recursive", content)
        self.assertIn("LTSEARCH_QUERY_LTEMBED_S3_PREFIX", content)
        self.assertNotIn("LTSEARCH_QUERY_EMBEDDING_PROVIDER", content)
        self.assertNotIn("LTSEARCH_BUILD_EMBEDDING_PROVIDER", content)
        self.assertIn('--template-file "$LTEMBED_E2E_TEMPLATE"', content)
        mode = LTEMBED_E2E_SCRIPT_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "ltembed e2e script must be executable")

    def test_ci_has_ltembed_e2e_job(self) -> None:
        content = WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("sam-ltembed-e2e:", content)
        self.assertIn("run-sam-ltembed-invoke-e2e.sh", content)
        self.assertIn("test_model_assets_packaging.py", content)


if __name__ == "__main__":
    unittest.main()
