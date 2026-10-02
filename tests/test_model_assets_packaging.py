import json
import stat
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
ASSETS_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-model-assets.sh"
ZIPS_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-lambda-zips.sh"
BUDGET_SCRIPT_PATH = REPO_ROOT / "scripts" / "check-lambda-size-budget.sh"
BUILDER_DOCKERFILE_PATH = REPO_ROOT / "sam" / "builder.Dockerfile"
BUILD_INFO_PATH = REPO_ROOT / "sam" / "ltembed-build-info.json"
FETCH_STATIC_LLAMA_PATH = REPO_ROOT / "scripts" / "fetch-static-llama.sh"
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
        # GGUF bundle provenance：逐源（model.gguf / tokenizer.json）URL + sha256，
        # pin 经 scripts/ltembed-pins.sh 从 builder.Dockerfile 单一来源读取。
        self.assertIn('"bundle_format": "gguf"', content)
        self.assertIn('"model.gguf"', content)
        self.assertIn('"tokenizer.json"', content)
        self.assertIn("scripts/ltembed-pins.sh", content)
        self.assertNotIn("bundle_url", content)
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
        # GGUF bundle：三件套必须在 manifest 中，model.gguf 校验 GGUF magic；
        # llama.cpp 静态链接进二进制，资产不再含原生库。
        for required in ("model.gguf", "tokenizer.json", "build-info.json"):
            self.assertIn(required, content)
        self.assertIn('b"GGUF"', content)
        self.assertNotIn("libonnxruntime", content)
        self.assertIn("model-assets", content)
        self.assertIn("manifest.json", content)
        for fn in ("query_lambda", "write_lambda", "index_builder_lambda"):
            self.assertIn(fn, content)
        mode = BUDGET_SCRIPT_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "budget script must be executable")

    def test_builder_dockerfile_pins_bundle_sha256_in_bundle_stage(self) -> None:
        content = BUILDER_DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertIn("AS bundle", content)
        bundle_stage = content[content.index("AS bundle") : content.index("AS builder")]
        self.assertIn("ARG LTEMBED_GGUF_SHA256=", bundle_stage)
        self.assertIn("ARG LTEMBED_TOKENIZER_SHA256=", bundle_stage)
        self.assertIn("sha256sum -c", bundle_stage)
        self.assertIn("/ltembed-assets/model.gguf", bundle_stage)
        self.assertIn("/ltembed-assets/build-info.json", bundle_stage)
        # HF resolve URL 必须按 commit 固定（resolve/main 是可变指针）。
        self.assertNotIn("/resolve/main/", bundle_stage)

    def test_builder_dockerfile_links_verified_static_llama_in_real_mode(self) -> None:
        content = BUILDER_DOCKERFILE_PATH.read_text(encoding="utf-8")
        builder_stage = content[content.index("AS builder") :]
        self.assertIn("ARG STATIC_LLAMA_URL=https://", builder_stage)
        self.assertIn("ARG STATIC_LLAMA_SHA256=", builder_stage)
        self.assertIn("fetch-static-llama.sh /opt/static-llama", builder_stage)
        # 取用必须早于 COPY . .，源码改动不使静态库层失效。
        self.assertLess(
            builder_stage.index("fetch-static-llama.sh /opt/static-llama"),
            builder_stage.index("\nCOPY . .\n"),
        )
        self.assertIn("STATIC_LLAMA_DIR=/opt/static-llama/extracted", builder_stage)

    def test_fetch_static_llama_verifies_tarball_sums_and_contract(self) -> None:
        content = FETCH_STATIC_LLAMA_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("sha256sum -c -", content)
        self.assertIn("sha256sum --quiet -c SHA256SUMS", content)
        self.assertIn("artifact_contract_version", content)
        self.assertIn("EXPECTED_CONTRACT=2", content)
        self.assertIn("sam/builder.Dockerfile", content)
        mode = FETCH_STATIC_LLAMA_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "fetch script must be executable")

    def test_build_info_declares_gguf_retrieval_bundle(self) -> None:
        info = json.loads(BUILD_INFO_PATH.read_text(encoding="utf-8"))
        meta = info["model_metadata"]
        self.assertEqual(info["target_id"], "jinaai/jina-embeddings-v5-text-nano-retrieval")
        self.assertEqual(meta["model_format"], "gguf")
        self.assertEqual(meta["pooling"], "last_token")
        self.assertEqual(meta["input_kind"], "retrieval")
        self.assertEqual(meta["query_prefix"], "Query: ")
        self.assertEqual(meta["document_prefix"], "Document: ")
        # quant 必须与 builder.Dockerfile 钉住的 GGUF 文件一致。
        builder = BUILDER_DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertIn(f"v5-nano-retrieval-{meta['quant']}.gguf", builder)

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
        # GGUF 引擎只认 bundle 目录；ORT 时代的 MODEL_PATH 已移除。
        self.assertNotIn("MODEL_PATH", content)
        self.assertNotIn("model.ort", content)
        # write 零模型依赖(AC-5:可独立部署)。
        write_block = content.split("WriteFunction:")[1].split("QueryFunction:")[0]
        self.assertNotIn("LTEMBED", write_block)

    def test_ltembed_e2e_script_covers_real_packaging_budget_and_invoke(self) -> None:
        self.assertTrue(LTEMBED_E2E_SCRIPT_PATH.exists())
        content = LTEMBED_E2E_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("prepare_locked_ltembed_checkout", content)
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
