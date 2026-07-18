import stat
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
PACKAGE_SCRIPT_PATH = REPO_ROOT / "scripts" / "package-lambda-zips.sh"
TEMPLATE_PATH = REPO_ROOT / "template.yaml"
ZIP_E2E_SCRIPT_PATH = REPO_ROOT / "scripts" / "e2e" / "run-sam-zip-invoke-e2e.sh"
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci.yml"


class LambdaZipPackagingTest(unittest.TestCase):
    def test_package_script_builds_in_al2023_and_stages_bootstrap(self) -> None:
        self.assertTrue(PACKAGE_SCRIPT_PATH.exists())
        content = PACKAGE_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("sam/builder.Dockerfile", content)
        self.assertIn("--platform linux/arm64", content)
        self.assertIn("bootstrap", content)
        self.assertIn("chmod +x", content)
        self.assertIn("zip -q -X", content)
        for fn in ("query_lambda", "write_lambda", "index_builder_lambda"):
            self.assertIn(fn, content)
        # 打包绝不在宿主机原生 cargo build（glibc 兼容性），也不引 cargo-lambda。
        self.assertNotIn("cargo-lambda", content)
        mode = PACKAGE_SCRIPT_PATH.stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR, "package script must be executable")

    def test_production_template_uses_zip_httpapi_and_sqs_redrive(self) -> None:
        self.assertTrue(TEMPLATE_PATH.exists())
        content = TEMPLATE_PATH.read_text(encoding="utf-8")
        self.assertIn("Transform: AWS::Serverless-2016-10-31", content)
        self.assertIn("Runtime: provided.al2023", content)
        self.assertIn("Handler: bootstrap", content)
        self.assertIn("arm64", content)
        self.assertNotIn("PackageType: Image", content)
        for code_uri in (
            "dist/write_lambda/",
            "dist/query_lambda/",
            "dist/index_builder_lambda/",
        ):
            self.assertIn(f"CodeUri: {code_uri}", content)
        self.assertIn("Type: HttpApi", content)
        self.assertIn("Type: SQS", content)
        self.assertIn("ReportBatchItemFailures", content)
        self.assertIn("RedrivePolicy", content)
        self.assertIn("deadLetterTargetArn", content)
        self.assertIn("VisibilityTimeout: 5400", content)
        self.assertIn("Timeout: 900", content)
        self.assertIn("LTSEARCH_BUILD_EMBEDDING_DIM", content)
        # #111 已交付模型 Layer,默认为 ltembed/512 生产档(#94 裁决);
        # stub e2e 经派生模板剥 Layer 并用 --env-vars 覆盖回 fixed/3。
        self.assertIn("Default: ltembed", content)
        self.assertIn("Default: '512'", content)
        self.assertIn("LTSEARCH_BUILD_FIXED_EMBEDDING", content)
        self.assertIn("LTSEARCH_QUERY_FIXED_EMBEDDING", content)

    def test_zip_e2e_script_covers_package_invoke_and_layout(self) -> None:
        self.assertTrue(ZIP_E2E_SCRIPT_PATH.exists())
        content = ZIP_E2E_SCRIPT_PATH.read_text(encoding="utf-8")
        self.assertIn("set -euo pipefail", content)
        self.assertIn("package-lambda-zips.sh", content)
        self.assertIn("assert_zip_layout", content)
        self.assertIn("make_apigw_event", content)
        self.assertIn("make_sqs_event", content)
        self.assertIn("batchItemFailures", content)
        self.assertIn('make_zip_e2e_template "$REPO_ROOT/template.yaml"', content)
        self.assertIn('--template-file "$ZIP_E2E_TEMPLATE"', content)
        self.assertNotIn('--template-file "$REPO_ROOT/template.yaml"', content)

    def test_ci_has_zip_e2e_job(self) -> None:
        content = WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("sam-zip-e2e:", content)
        self.assertIn("run-sam-zip-invoke-e2e.sh", content)


if __name__ == "__main__":
    unittest.main()
