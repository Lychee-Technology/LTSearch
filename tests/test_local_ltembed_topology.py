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
QUERY_SEMANTIC_FIXTURE_PATH = (
    REPO_ROOT / "tests" / "fixtures" / "e2e" / "query_request_semantic.json"
)
WRITE_REQUEST_PATH = REPO_ROOT / "tests" / "fixtures" / "e2e" / "write_request.json"
DEGRADED_OVERLAY_PATH = REPO_ROOT / "docker-compose.local-ltembed.degraded.yml"
DEGRADED_RUNNER_PATH = (
    REPO_ROOT / "scripts" / "e2e" / "run-local-real-degraded-health.sh"
)


class LocalHttpLibTest(unittest.TestCase):
    """#141: 可复用本地 HTTP 黑盒库的接口守卫（#142/#143 依赖这些函数名）。"""

    def test_http_lib_exposes_reusable_interface(self) -> None:
        self.assertTrue(HTTP_LIB_PATH.exists(), f"missing: {HTTP_LIB_PATH}")
        text = HTTP_LIB_PATH.read_text(encoding="utf-8")
        for function in [
            "lhttp_init()",
            "lhttp_compose()",
            "lhttp_up()",
            "lhttp_up_nowait()",
            "lhttp_down()",
            "lhttp_wait_http_ready()",
            "lhttp_port()",
            "lhttp_request()",
            "lhttp_assert_status()",
            "lhttp_assert_health()",
            "lhttp_wait_index_version()",
            "lhttp_dump_diagnostics()",
            "lhttp_finish()",
        ]:
            self.assertIn(function, text, f"http lib lost function: {function}")

    def test_http_lib_teardown_is_unconditional_and_not_swallowed(self) -> None:
        text = HTTP_LIB_PATH.read_text(encoding="utf-8")
        self.assertIn("down -v --remove-orphans", text)
        # teardown 失败必须传播（P1）：不允许回到 `lhttp_down || true` 的写法。
        self.assertNotIn("lhttp_down || true", text)
        self.assertIn('exit_code="$down_rc"', text)

    def test_http_lib_polling_records_payloads(self) -> None:
        # 版本轮询必须经 lhttp_request 落盘（P2），不得绕过载荷记录直连 curl。
        text = HTTP_LIB_PATH.read_text(encoding="utf-8")
        self.assertIn("lhttp_request poll-index-version", text)


class HttpLibReadyTimeoutBehaviorTest(unittest.TestCase):
    """#142: lhttp_wait_http_ready 的超时契约必须对「挂死对端」成立。

    源码字符串 guard 覆盖不了这个失败模式：端口 accept 但永不响应 HTTP 时，
    无界 curl 会在第一次请求上永久阻塞，声明的 timeout_s 永远轮不到检查。
    这里起一个只 bind+listen、不 accept 的本地 socket（内核 backlog 会完成
    TCP 握手），真实调用 helper 验证它在预算内以非零退出。
    """

    def test_wait_http_ready_fails_within_budget_against_silent_peer(self) -> None:
        import socket
        import subprocess
        import tempfile
        import time

        listener = socket.socket()
        try:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            port = listener.getsockname()[1]
            with tempfile.TemporaryDirectory() as run_dir:
                script = (
                    "set -euo pipefail\n"
                    f'source "{HTTP_LIB_PATH}"\n'
                    f'LHTTP_RUN_DIR="{run_dir}"\n'
                    # 单次 2s、总预算 5s：挂死对端下必须在 ~5s+一次请求内返回。
                    "LHTTP_READY_MAX_TIME=2 "
                    f'lhttp_wait_http_ready ready-probe "http://127.0.0.1:{port}/health" 5\n'
                )
                start = time.monotonic()
                proc = subprocess.run(
                    ["bash", "-c", script],
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                elapsed = time.monotonic() - start
            self.assertNotEqual(
                proc.returncode,
                0,
                f"helper must fail against a silent peer: {proc.stderr}",
            )
            self.assertIn("not HTTP-reachable", proc.stderr)
            # 预算 5s + 最后一次请求(≤2s) + sleep 与进程开销的余量。
            self.assertLess(elapsed, 30, "timeout budget was not honored")
        finally:
            listener.close()


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


def _compose_without_comments(path: Path = COMPOSE_PATH) -> str:
    lines = path.read_text(encoding="utf-8").splitlines()
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
        self.assertIn('LTSEARCH_BUILD_EMBEDDING_DIM: "512"', text)
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
        write_request = json.loads(WRITE_REQUEST_PATH.read_text(encoding="utf-8"))
        # 真实模型语义排序有抖动：top_k 必须覆盖全部写入文档，断言只做成员检查。
        self.assertGreaterEqual(fixture["top_k"], len(write_request["documents"]))

    def test_semantic_fixture_has_zero_lexical_overlap(self) -> None:
        # 该 fixture 是"真实 embedding 生效"的判别器：embedding 失败时 query 回退
        # tantivy keyword-only（src/query/router.rs 的 search_keyword_only 分支），
        # 查询与所有文档零词面重叠 ⇒ 回退路径必返回 0 条。此性质被破坏则 runner
        # 的语义断言失去判别力。
        fixture = json.loads(QUERY_SEMANTIC_FIXTURE_PATH.read_text(encoding="utf-8"))
        write_request = json.loads(WRITE_REQUEST_PATH.read_text(encoding="utf-8"))
        query_tokens = set(re.findall(r"[a-z0-9]+", fixture["query"].lower()))
        self.assertTrue(query_tokens)
        for document in write_request["documents"]:
            doc_tokens = set(re.findall(r"[a-z0-9]+", document["text"].lower()))
            overlap = query_tokens & doc_tokens
            self.assertFalse(
                overlap,
                f"semantic fixture overlaps doc {document['doc_id']}: {sorted(overlap)}",
            )
        self.assertGreaterEqual(fixture["top_k"], len(write_request["documents"]))


class DegradedOverlayTest(unittest.TestCase):
    """#142: 降级 bundle overlay 只许覆盖 env，且 write 必须保持健康。"""

    def test_overlay_only_overrides_bundle_env(self) -> None:
        self.assertTrue(
            DEGRADED_OVERLAY_PATH.exists(), f"missing: {DEGRADED_OVERLAY_PATH}"
        )
        text = _compose_without_comments(DEGRADED_OVERLAY_PATH)
        # 拓扑结构（端口/卷/网络/healthcheck/固定 name）必须全部继承 base，
        # overlay 一旦声明这些就会破坏 project 隔离或 healthcheck 语义。
        for forbidden in ["write:", "ports:", "volumes:", "networks:", "healthcheck", "name:"]:
            self.assertNotIn(
                forbidden, text, f"degraded overlay must not declare {forbidden}"
            )

    def test_overlay_covers_missing_and_broken_branches(self) -> None:
        text = _compose_without_comments(DEGRADED_OVERLAY_PATH)
        # query＝缺失分支：bundle 路径不存在，存在性预检直接失败。
        self.assertIn("LTSEARCH_QUERY_LTEMBED_BUNDLE_DIR: /nonexistent", text)
        self.assertIn(
            "LTSEARCH_QUERY_LTEMBED_MODEL_PATH: /nonexistent/model.ort", text
        )
        # build＝损坏分支：目录存在（/app）但缺 tokenizer.json 等 bundle 文件，
        # 在 LTEmbed require_file 处确定失败。MODEL_PATH 不得覆盖——保持
        # /opt/ltembed/model.ort 让存在性预检通过，才走到「存在但非法」分支。
        self.assertIn("LTSEARCH_BUILD_LTEMBED_BUNDLE_DIR: /app", text)
        self.assertNotIn("LTSEARCH_BUILD_LTEMBED_MODEL_PATH", text)


class DegradedRunnerTest(unittest.TestCase):
    """#142: 降级 runner 必须 no-wait 启动并断言 200/503/503 分角色契约。"""

    def test_runner_uses_lib_with_nowait_and_teardown(self) -> None:
        self.assertTrue(
            DEGRADED_RUNNER_PATH.exists(), f"missing: {DEGRADED_RUNNER_PATH}"
        )
        text = DEGRADED_RUNNER_PATH.read_text(encoding="utf-8")
        self.assertIn("scripts/e2e/local_http_lib.sh", text)
        self.assertIn("trap 'lhttp_finish $?' EXIT", text)
        self.assertIn("docker-compose.local-ltembed.degraded.yml", text)
        self.assertIn("lhttp_up_nowait", text)
        # degraded 的 query/build healthcheck 注定 unhealthy，`up -d --wait`
        # 必然失败：runner 不得出现任何非 no-wait 的 lhttp_up 调用。
        self.assertIsNone(
            re.search(r"lhttp_up(?!_nowait)", text),
            "degraded runner must not call plain lhttp_up (--wait would fail)",
        )
        # write 后台 worker 必须关闭：degraded write 的 job 在 build 侧必然
        # 失败并重试进 dead_jobs，只会制造日志噪音。
        self.assertIn("LTSEARCH_BUILD_WORKER_ENABLED=false", text)

    def test_runner_asserts_per_role_status_contract(self) -> None:
        text = DEGRADED_RUNNER_PATH.read_text(encoding="utf-8")
        self.assertEqual(
            text.count("lhttp_assert_status 503"),
            2,
            "query 与 build 的 /health 各断言一次 503",
        )
        self.assertEqual(
            text.count("lhttp_assert_status 200"),
            2,
            "write /health 与降级下的 POST /write 各断言一次 200",
        )
        # 503 断言必须锁 detail 的 bundle_dir= 前缀（证明来自 bundle probe），
        # 且两个角色分别对应缺失/损坏两个分支。
        self.assertIn("bundle_dir=/nonexistent", text)
        self.assertIn("bundle_dir=/app", text)
        # 先等 HTTP 可达再断言状态码，避免 no-wait 启动竞态。
        self.assertLess(
            text.index("lhttp_wait_http_ready"),
            text.index("lhttp_assert_status 503"),
        )


if __name__ == "__main__":
    unittest.main()
