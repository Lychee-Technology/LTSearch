import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DOCKERFILE_PATH = REPO_ROOT / "sam" / "local.Dockerfile"
MMAP_INDEX_PATH = REPO_ROOT / "src" / "index" / "mmap_index.rs"

# The forbidden tokens are assembled from fragments so this guard file itself
# stays clean under the repo's `grep` gate for the retired directory-convention
# identifiers (otherwise the assertions below would match themselves).
IMAGE_STATIC_PATH = "/app/" + "static"
STATIC_DIR_ENV = "LTSEARCH_QUERY_" + "STATIC_DIR"
IMAGE_STATIC_CONST = "IMAGE_STATIC" + "_DIR"


class ImageNoStaticConventionTest(unittest.TestCase):
    """Guards that the implicit `static/` directory convention is fully gone.

    Static retrieval now resolves exclusively through the activation pointer
    (`static/_head` -> `static/releases/<id>/`), so the runtime image
    (`sam/local.Dockerfile`, the released unified local image since #113) must
    not bake an image static layer nor advertise the static-dir env override,
    and the mmap index must not hardcode an image static directory constant.
    """

    def test_dockerfile_has_no_static_directory_convention(self) -> None:
        self.assertTrue(
            DOCKERFILE_PATH.exists(), f"missing Dockerfile: {DOCKERFILE_PATH}"
        )
        contents = DOCKERFILE_PATH.read_text(encoding="utf-8")
        self.assertNotIn(
            IMAGE_STATIC_PATH,
            contents,
            "Dockerfile must not bake an image static index layer",
        )
        self.assertNotIn(
            STATIC_DIR_ENV,
            contents,
            "Dockerfile must not set the static-dir env override",
        )

    def test_mmap_index_has_no_image_static_dir(self) -> None:
        self.assertTrue(
            MMAP_INDEX_PATH.exists(), f"missing mmap_index.rs: {MMAP_INDEX_PATH}"
        )
        contents = MMAP_INDEX_PATH.read_text(encoding="utf-8")
        self.assertNotIn(
            IMAGE_STATIC_CONST,
            contents,
            "mmap_index.rs must not hardcode an image static directory constant",
        )


if __name__ == "__main__":
    unittest.main()
