import json
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURES_DIR = REPO_ROOT / "tests" / "fixtures" / "e2e"
DOCUMENTS_PATH = FIXTURES_DIR / "documents.json"
WRITE_REQUEST_PATH = FIXTURES_DIR / "write_request.json"
QUERY_REQUEST_PATH = FIXTURES_DIR / "query_request.json"
LIB_SH_PATH = REPO_ROOT / "scripts" / "e2e" / "lib.sh"


class E2EFixturesTest(unittest.TestCase):
    def test_documents_fixture_has_required_shape_and_coverage(self) -> None:
        self.assertTrue(
            DOCUMENTS_PATH.exists(), f"missing documents fixture: {DOCUMENTS_PATH}"
        )

        documents = json.loads(DOCUMENTS_PATH.read_text(encoding="utf-8"))
        self.assertIsInstance(documents, list)
        self.assertGreaterEqual(len(documents), 5)
        self.assertLessEqual(len(documents), 6)

        doc_ids = set()
        tenants = set()
        categories = set()

        for document in documents:
            self.assertIn("doc_id", document)
            self.assertIn("text", document)
            self.assertIn("metadata", document)
            self.assertIsInstance(document["metadata"], dict)

            metadata = document["metadata"]
            self.assertIn("lang", metadata)
            self.assertIn("category", metadata)
            self.assertIn("tenant", metadata)

            doc_ids.add(document["doc_id"])
            tenants.add(metadata["tenant"])
            categories.add(metadata["category"])

        self.assertEqual(len(doc_ids), len(documents))
        self.assertGreaterEqual(len(tenants), 2)
        self.assertIn("hybrid", categories)
        self.assertIn("keyword", categories)
        self.assertIn("noise", categories)

    def test_request_templates_reference_fixture_data(self) -> None:
        self.assertTrue(
            WRITE_REQUEST_PATH.exists(),
            f"missing write request fixture: {WRITE_REQUEST_PATH}",
        )
        self.assertTrue(
            QUERY_REQUEST_PATH.exists(),
            f"missing query request fixture: {QUERY_REQUEST_PATH}",
        )

        write_request = json.loads(WRITE_REQUEST_PATH.read_text(encoding="utf-8"))
        query_request = json.loads(QUERY_REQUEST_PATH.read_text(encoding="utf-8"))

        self.assertIn("operation", write_request)
        self.assertEqual(write_request["operation"], "ingest")
        self.assertIn("documents", write_request)
        self.assertGreaterEqual(len(write_request["documents"]), 5)

        self.assertEqual(query_request["query"], "rust retrieval")
        self.assertEqual(query_request["top_k"], 3)
        self.assertTrue(query_request["include_metadata"])

    def test_shared_shell_helper_contains_expected_e2e_functions(self) -> None:
        self.assertTrue(
            LIB_SH_PATH.exists(), f"missing E2E shell helper: {LIB_SH_PATH}"
        )

        content = LIB_SH_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("wait_for_moto()", content)
        self.assertIn("create_e2e_bucket()", content)
        self.assertIn("create_e2e_queue()", content)
        self.assertIn("prepare_local_ltembed_checkout()", content)
        self.assertIn("receive_one_sqs_batch()", content)
        self.assertIn("sync_e2e_artifacts_from_moto()", content)
        self.assertIn("assert_json_field()", content)


if __name__ == "__main__":
    unittest.main()
