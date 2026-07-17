import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
START_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "start-sam-moto.sh"
HTTP_FLOW_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "run-http-flow.sh"
STOP_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "stop-sam-moto.sh"


class SamStartApiE2ETest(unittest.TestCase):
    def test_start_script_sets_up_moto_and_sam_api(self) -> None:
        self.assertTrue(START_SCRIPT_PATH.exists(), f"missing: {START_SCRIPT_PATH}")

        content = START_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn('source "$(dirname "$0")/lib.sh"', content)
        self.assertIn("wait_for_moto", content)
        self.assertIn("create_e2e_bucket", content)
        self.assertIn("create_e2e_queue", content)
        self.assertIn("sam local start-api", content)
        self.assertIn("--docker-network ltsearch-e2e", content)
        self.assertIn("--env-vars", content)
        self.assertIn("sam-api.pid", content)
        self.assertIn("http://localhost:3000", content)
        self.assertIn("e2e-state.json", content)
        self.assertIn("LTSEARCH_BUILD_EMBEDDING_DIM", content)
        self.assertNotIn("host.docker.internal", content)

    def test_http_flow_script_runs_write_build_query(self) -> None:
        self.assertTrue(HTTP_FLOW_SCRIPT_PATH.exists(), f"missing: {HTTP_FLOW_SCRIPT_PATH}")

        content = HTTP_FLOW_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn('source "$(dirname "$0")/lib.sh"', content)
        self.assertIn("POST /write", content)
        self.assertIn("POST /query", content)
        self.assertIn("invoke BuildFunction", content)
        self.assertIn("receive_one_sqs_batch", content)
        self.assertIn("assert_json_field", content)
        self.assertIn("accepted_count", content)
        # builder is invoked with an SQS batch envelope and returns partial-batch
        # failures; success is an empty batchItemFailures list.
        self.assertIn("make_sqs_event", content)
        self.assertIn("batchItemFailures", content)
        self.assertIn("doc-rust-hybrid", content)
        self.assertIn("e2e-state.json", content)
        self.assertIn("--docker-network ltsearch-e2e", content)

    def test_stop_script_cleans_up_processes_and_containers(self) -> None:
        self.assertTrue(STOP_SCRIPT_PATH.exists(), f"missing: {STOP_SCRIPT_PATH}")

        content = STOP_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("sam-api.pid", content)
        self.assertIn("docker compose", content)
        self.assertIn("docker-compose.moto.yml", content)
        self.assertIn("down -v", content)

    def test_scripts_are_executable(self) -> None:
        import os
        for path in [START_SCRIPT_PATH, HTTP_FLOW_SCRIPT_PATH, STOP_SCRIPT_PATH]:
            self.assertTrue(
                os.access(path, os.X_OK),
                f"{path.name} is not executable",
            )

    def test_http_flow_reuses_shared_fixtures(self) -> None:
        content = HTTP_FLOW_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("write_request.json", content)
        self.assertIn("query_request.json", content)

    def test_http_flow_query_assertions_match_split_response_contract(self) -> None:
        content = HTTP_FLOW_SCRIPT_PATH.read_text(encoding="utf-8")

        self.assertIn("dynamic_count", content)
        self.assertIn("dynamic_chunks", content)
        self.assertNotIn("total_count", content)
        self.assertNotIn("response['results']", content)


if __name__ == "__main__":
    unittest.main()
