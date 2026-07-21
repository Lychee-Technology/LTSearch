import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
HTTP_LIB_PATH = REPO_ROOT / "scripts" / "e2e" / "local_http_lib.sh"


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


if __name__ == "__main__":
    unittest.main()
