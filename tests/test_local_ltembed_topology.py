import json
import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
HTTP_LIB_PATH = REPO_ROOT / "scripts" / "e2e" / "local_http_lib.sh"
DOCKERFILE_PATH = REPO_ROOT / "sam" / "local-ltembed.Dockerfile"
FIXED_DOCKERFILE_PATH = REPO_ROOT / "sam" / "local.Dockerfile"
BUILDER_DOCKERFILE_PATH = REPO_ROOT / "sam" / "builder.Dockerfile"
COMPOSE_PATH = REPO_ROOT / "docker-compose.local-ltembed.yml"
BUILD_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "build-local-ltembed-image.sh"
QUERY_REAL_FIXTURE_PATH = (
    REPO_ROOT / "tests" / "fixtures" / "e2e" / "query_request_real.json"
)
RUNNER_PATH = REPO_ROOT / "scripts" / "e2e" / "run-local-real-flow.sh"


class LocalHttpLibTest(unittest.TestCase):
    """#141: 可复用本地 HTTP 黑盒库的接口守卫（#142/#143 依赖这些函数名）。"""

    def test_http_lib_exposes_reusable_interface(self) -> None:
        self.assertTrue(HTTP_LIB_PATH.exists(), f"missing: {HTTP_LIB_PATH}")
        text = HTTP_LIB_PATH.read_text(encoding="utf-8")
        for function in [
            "lhttp_init()",
            "lhttp_compose()",
            "lhttp_up()",
            "lhttp_down()",
            "lhttp_port()",
            "lhttp_request()",
            "lhttp_assert_status()",
            "lhttp_assert_health()",
            "lhttp_wait_index_version()",
            "lhttp_dump_diagnostics()",
            "lhttp_finish()",
        ]:
            self.assertIn(function, text, f"http lib lost function: {function}")

    def test_http_lib_teardown_is_unconditional(self) -> None:
        text = HTTP_LIB_PATH.read_text(encoding="utf-8")
        self.assertIn("down -v --remove-orphans", text)


class LocalLtembedImageTest(unittest.TestCase):
    """#141: real 镜像构建约束——local,ltembed 特性、bundle 烘焙、pin 单一来源。"""

    def test_dockerfile_builds_real_local_image(self) -> None:
        self.assertTrue(DOCKERFILE_PATH.exists(), f"missing: {DOCKERFILE_PATH}")
        text = DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertIn("--features local,ltembed --bin ltsearch", text)
        self.assertIn("COPY --from=bundle /ltembed-assets /opt/ltembed", text)
        self.assertIn(
            'ltembed = { path = "/src/.sam-local-deps/LTEmbed" }', text
        )
        self.assertIn("sha256sum -c", text)
        self.assertIn('ENTRYPOINT ["/app/ltsearch"]', text)

    def test_dockerfile_has_no_hardcoded_bundle_pin(self) -> None:
        # pin 权威只在 sam/builder.Dockerfile；本文件 ARG 必须留空，由构建脚本注入。
        text = DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertIn("ARG LTEMBED_BUNDLE_URL=\n", text)
        self.assertIn("ARG LTEMBED_BUNDLE_SHA256=\n", text)
        self.assertIsNone(
            re.search(r"LTEMBED_BUNDLE_SHA256=[0-9a-f]{64}", text),
            "bundle SHA256 must not be hardcoded outside sam/builder.Dockerfile",
        )

    def test_build_script_injects_pin_from_builder_dockerfile(self) -> None:
        self.assertTrue(BUILD_SCRIPT_PATH.exists(), f"missing: {BUILD_SCRIPT_PATH}")
        text = BUILD_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("prepare_locked_ltembed_checkout", text)
        self.assertIn(
            "sed -n 's/^ARG LTEMBED_BUNDLE_URL=//p'", text
        )
        self.assertIn(
            "sed -n 's/^ARG LTEMBED_BUNDLE_SHA256=//p'", text
        )
        self.assertIn("--platform linux/arm64", text)

    def test_fixed_local_dockerfile_is_untouched(self) -> None:
        # AC-1 回归守卫：发布镜像仍是 fixed（stub patch + --features local）。
        text = FIXED_DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertIn("--features local --bin ltsearch", text)
        self.assertIn('ltembed = { path = "/src/vendor/ltembed-stub" }', text)
        self.assertNotIn("ltembed-assets", text)

    def test_base_image_pins_stay_aligned(self) -> None:
        # digest pin 与 releasever 锁必须与 builder.Dockerfile 一致（bump 一起改）。
        builder = BUILDER_DOCKERFILE_PATH.read_text(encoding="utf-8")
        real = DOCKERFILE_PATH.read_text(encoding="utf-8")
        digest = re.search(r"amazonlinux:2023@sha256:[0-9a-f]{64}", builder)
        assert digest is not None
        self.assertIn(digest.group(0), real)
        releasever = re.search(r'echo "[0-9.]+" > /etc/dnf/vars/releasever', builder)
        assert releasever is not None
        self.assertIn(releasever.group(0), real)


def _compose_without_comments() -> str:
    lines = COMPOSE_PATH.read_text(encoding="utf-8").splitlines()
    return "\n".join(
        line for line in lines if not line.lstrip().startswith("#")
    )


class LocalLtembedComposeTest(unittest.TestCase):
    """#141: Compose 拓扑必须按 project 隔离——无固定 name:、无固定 host 端口。"""

    def test_compose_topology_is_run_isolated(self) -> None:
        self.assertTrue(COMPOSE_PATH.exists(), f"missing: {COMPOSE_PATH}")
        text = _compose_without_comments()
        self.assertNotIn("name:", text, "卷/网络名必须由 compose project 派生")
        self.assertEqual(
            text.count('"127.0.0.1::8080"'),
            3,
            "三个角色都必须用 loopback 临时端口（build 也要暴露 /health）",
        )
        self.assertNotIn("19080", text)
        self.assertNotIn("19081", text)

    def test_compose_roles_share_one_local_root_with_real_model(self) -> None:
        text = _compose_without_comments()
        self.assertEqual(text.count("ltsearch-data:/var/lib/ltsearch"), 3)
        self.assertIn("LTSEARCH_BUILD_EMBEDDING_PROVIDER: ltembed", text)
        self.assertIn("LTSEARCH_QUERY_EMBEDDING_PROVIDER: ltembed", text)
        self.assertIn("LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR: /opt/ltembed", text)
        self.assertIn("LTSEARCH_QUERY_LTEMBED_MODEL_PATH: /opt/ltembed/model.ort", text)
        for forbidden in ["moto", "AWS_", "_S3_"]:
            self.assertNotIn(forbidden, text, f"real 拓扑不得引用 {forbidden}")
        self.assertIn(
            "LTSEARCH_BUILD_WORKER_ENABLED: ${LTSEARCH_BUILD_WORKER_ENABLED:-true}",
            text,
        )

    def test_runner_uses_reusable_lib_with_teardown_trap(self) -> None:
        self.assertTrue(RUNNER_PATH.exists(), f"missing: {RUNNER_PATH}")
        text = RUNNER_PATH.read_text(encoding="utf-8")
        self.assertIn("scripts/e2e/local_http_lib.sh", text)
        self.assertIn("trap 'lhttp_finish $?' EXIT", text)
        self.assertIn("lhttp_assert_health health-build", text)
        self.assertIn("lhttp_assert_health health-query", text)
        self.assertIn("lhttp_wait_index_version", text)
        self.assertIn("query_request_real.json", text)

    def test_query_real_fixture_covers_all_docs(self) -> None:
        fixture = json.loads(QUERY_REAL_FIXTURE_PATH.read_text(encoding="utf-8"))
        write_request = json.loads(
            (REPO_ROOT / "tests" / "fixtures" / "e2e" / "write_request.json").read_text(
                encoding="utf-8"
            )
        )
        # 真实模型语义排序有抖动：top_k 必须覆盖全部写入文档，断言只做成员检查。
        self.assertGreaterEqual(fixture["top_k"], len(write_request["documents"]))


if __name__ == "__main__":
    unittest.main()
