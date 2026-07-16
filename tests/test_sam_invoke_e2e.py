import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"
SAM_TEMPLATE_PATH = REPO_ROOT / "template.sam-e2e.yaml"
INVOKE_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "run-sam-local-invoke-e2e.sh"
WRITE_DOCKERFILE_PATH = REPO_ROOT / "sam" / "write_lambda.Dockerfile"
BUILD_DOCKERFILE_PATH = REPO_ROOT / "sam" / "index_builder_lambda.Dockerfile"
QUERY_DOCKERFILE_PATH = REPO_ROOT / "sam" / "query_lambda.Dockerfile"


class SamInvokeE2ETest(unittest.TestCase):
    def test_sam_template_defines_three_lambdas_for_local_e2e(self) -> None:
        self.assertTrue(
            SAM_TEMPLATE_PATH.exists(), f"missing SAM template: {SAM_TEMPLATE_PATH}"
        )

        content = SAM_TEMPLATE_PATH.read_text(encoding="utf-8")
        self.assertIn("Transform: AWS::Serverless-2016-10-31", content)
        self.assertIn("WriteFunction:", content)
        self.assertIn("BuildFunction:", content)
        self.assertIn("QueryFunction:", content)
        self.assertIn("PackageType: Image", content)
        self.assertIn("sam/write_lambda.Dockerfile", content)
        self.assertIn("sam/index_builder_lambda.Dockerfile", content)
        self.assertIn("sam/query_lambda.Dockerfile", content)
        self.assertIn("LTSEARCH_WRITE_S3_BUCKET", content)
        self.assertIn("LTSEARCH_BUILD_S3_BUCKET", content)
        self.assertIn("LTSEARCH_QUERY_ARTIFACT_ROOT", content)
        self.assertIn("/tmp/ltsearch-e2e-artifacts", content)
        self.assertIn("http://moto:5000", content)
        self.assertNotIn("host.docker.internal", content)

    def test_invoke_e2e_script_has_expected_flow_steps(self) -> None:
        self.assertTrue(
            INVOKE_SCRIPT_PATH.exists(),
            f"missing invoke E2E script: {INVOKE_SCRIPT_PATH}",
        )

        content = INVOKE_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn('source "$(dirname "$0")/lib.sh"', content)
        self.assertIn('SAM_BUILD_LOG="$E2E_OUTPUT_DIR/sam-build.log"', content)
        self.assertIn(
            'SAM_BUILD_DOCKER_EVENTS_LOG="$E2E_OUTPUT_DIR/sam-build-docker-events.log"',
            content,
        )
        self.assertIn(
            'run_with_heartbeat "sam build" "$SAM_BUILD_LOG" "$SAM_BUILD_DOCKER_EVENTS_LOG" sam build --debug --template-file "$SAM_SOURCE_TEMPLATE"',
            content,
        )
        self.assertIn('--env-vars "$ENV_VARS_JSON"', content)
        self.assertIn("--docker-network ltsearch-e2e", content)
        self.assertIn("sam local invoke WriteFunction", content)
        self.assertIn("sam local invoke BuildFunction", content)
        self.assertIn("sam local invoke QueryFunction", content)
        self.assertIn("wait_for_moto", content)
        self.assertIn("create_e2e_bucket", content)
        self.assertIn("create_e2e_queue", content)
        self.assertIn("receive_one_sqs_batch", content)
        self.assertIn("ENV_VARS_JSON", content)
        self.assertIn("http://moto:5000", content)
        self.assertNotIn("host.docker.internal", content)

    def test_e2e_helpers_keep_long_sam_builds_alive_in_ci(self) -> None:
        helpers = (REPO_ROOT / "scripts" / "e2e" / "lib.sh").read_text(encoding="utf-8")

        self.assertIn("run_with_heartbeat()", helpers)
        self.assertIn('local log_file="$1"', helpers)
        self.assertIn('local docker_events_log="$3"', helpers)
        self.assertIn('tee "$log_file"', helpers)
        self.assertIn('echo "$label still running..."', helpers)
        self.assertIn('tail_log_snapshot "$log_file"', helpers)
        self.assertIn('start_docker_events_capture "$docker_events_log"', helpers)
        self.assertIn("stop_docker_events_capture", helpers)
        self.assertIn("LTSEARCH_E2E_HEARTBEAT_SECONDS", helpers)

    def test_ci_workflow_includes_separate_sam_e2e_job(self) -> None:
        self.assertTrue(WORKFLOW_PATH.exists(), f"missing workflow: {WORKFLOW_PATH}")

        content = WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("sam-e2e:", content)
        self.assertIn("needs: integration", content)
        self.assertIn("bash scripts/e2e/run-sam-local-invoke-e2e.sh", content)
        self.assertIn("docker compose -f docker-compose.moto.yml up -d", content)
        self.assertIn("docker compose -f docker-compose.moto.yml down -v", content)

    def test_sam_dockerfiles_use_explicit_arm_images(self) -> None:
        builder_path = REPO_ROOT / "sam" / "builder.Dockerfile"
        builder_content = builder_path.read_text(encoding="utf-8")
        self.assertIn(
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023",
            builder_content,
        )
        # #130：FROM 不得携带常量 --platform（FromPlatformFlagConstDisallowed），
        # arm64 由构建调用方以 `docker build --platform linux/arm64` 显式指定。
        self.assertNotIn("--platform", builder_content)
        self.assertIn("cargo build --release --no-default-features", builder_content)

        for dockerfile_path in [
            WRITE_DOCKERFILE_PATH,
            BUILD_DOCKERFILE_PATH,
            QUERY_DOCKERFILE_PATH,
        ]:
            content = dockerfile_path.read_text(encoding="utf-8")
            self.assertIn("ltsearch-e2e-builder", content, dockerfile_path.as_posix())
            self.assertIn("FROM public.ecr.aws/lambda/provided:al2023-arm64", content)

    def test_builder_dockerfile_downloads_ltembed_bundle(self) -> None:
        builder_path = REPO_ROOT / "sam" / "builder.Dockerfile"
        content = builder_path.read_text(encoding="utf-8")

        self.assertIn("ARG LTEMBED_BUNDLE_URL=", content)
        self.assertIn("model.ort", content)
        self.assertIn("tokenizer.json", content)
        self.assertIn("build-info.json", content)
        self.assertIn("libonnxruntime.so", content)
        # defaults to the public pinned minimal-ort-builder release asset
        self.assertIn(
            "minimal-ort-builder/releases/download/v1.0.9/", content
        )
        # real mode still fails loudly if the URL is explicitly emptied
        self.assertIn("requires LTEMBED_BUNDLE_URL", content)
        # download must happen before COPY so the layer is cached independently
        download_pos = content.index("LTEMBED_BUNDLE_URL")
        copy_pos = content.index("COPY . .")
        self.assertLess(download_pos, copy_pos)

    def test_ltembed_env_vars_wired_in_sam_template(self) -> None:
        content = SAM_TEMPLATE_PATH.read_text(encoding="utf-8")

        self.assertIn("LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR", content)
        self.assertIn("LTSEARCH_BUILD_LTEMBED_MODEL_PATH", content)
        self.assertIn("LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR", content)
        self.assertIn("LTSEARCH_QUERY_LTEMBED_MODEL_PATH", content)
        self.assertIn("/ltembed-assets/model.ort", content)
        # legacy candle-era wiring must be gone
        self.assertNotIn("LTEMBED_CONFIG_PATH", content)
        self.assertNotIn("LTEMBED_POOLING", content)
        self.assertNotIn("model.safetensors", content)

    def test_builder_dockerfile_supports_ltembed_mode(self) -> None:
        builder_path = REPO_ROOT / "sam" / "builder.Dockerfile"
        content = builder_path.read_text(encoding="utf-8")

        self.assertIn("ARG LTEMBED_MODE=stub", content)
        self.assertIn("ltembed-stub", content)
        self.assertIn(".sam-local-deps/LTEmbed", content)
        # real mode enables ltembed per profile (lambda bins vs server bins);
        # the feature is composed with the profile, not passed bare.
        self.assertIn("--features lambda,ltembed", content)
        self.assertIn("--features aws,ltembed", content)
        self.assertIn("/ltembed-assets", content)
        self.assertNotIn("stage_ltembed_assets", content)
        self.assertNotIn(".sam-local-deps/ltembed-assets", content)

    def test_build_and_query_dockerfiles_copy_ltembed_assets(self) -> None:
        for dockerfile_path in [BUILD_DOCKERFILE_PATH, QUERY_DOCKERFILE_PATH]:
            content = dockerfile_path.read_text(encoding="utf-8")
            self.assertIn(
                "COPY --from=builder /ltembed-assets /ltembed-assets",
                content,
                dockerfile_path.as_posix(),
            )

    def test_ltembed_scenarios_are_asset_gated(self) -> None:
        content = INVOKE_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("LTSEARCH_E2E_LTEMBED", content)
        self.assertIn("LTEMBED_MODE=real", content)
        self.assertIn("LTSEARCH_E2E_LTEMBED_BUNDLE_URL", content)
        # the real-mode Docker build patches ltembed to .sam-local-deps/LTEmbed,
        # so the checkout must be staged before building
        self.assertIn("prepare_local_ltembed_checkout", content)
        self.assertIn("embedding_dim': 512", content)
        self.assertIn("env-vars-ltembed.json", content)
        self.assertIn("LTSEARCH_BUILD_EMBEDDING_PROVIDER", content)
        self.assertIn("LTSEARCH_QUERY_EMBEDDING_PROVIDER", content)
        self.assertIn("ltembed-build-event.json", content)
        # LTEmbed block must come after the fixed-embedding assertions
        fixed_end = content.index("assert 'doc-rust-hybrid' in doc_ids")
        ltembed_gate = content.index("LTSEARCH_E2E_LTEMBED")
        self.assertGreater(ltembed_gate, fixed_end)

    def test_query_assertions_match_split_response_contract(self) -> None:
        content = INVOKE_SCRIPT_PATH.read_text(encoding="utf-8")

        self.assertIn("dynamic_count", content)
        self.assertIn("dynamic_chunks", content)
        self.assertNotIn("total_count", content)
        self.assertNotIn("response['results']", content)


if __name__ == "__main__":
    unittest.main()
