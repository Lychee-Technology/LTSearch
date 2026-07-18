#!/usr/bin/env bash
# Lambda 尺寸预算守卫（#111，AC-3）：
# 1) 三个函数 ZIP 各自解压 ≤ 250MB（Lambda 单函数硬限；#111 实测未 strip 的
#    real 二进制 235.7 MiB，距硬限仅 ~14 MiB —— strip 由 package-lambda-zips.sh
#    强制），>200MB 打 warning；bootstrap 必须为 AArch64 ELF（e_machine == 0xB7，
#    不依赖宿主机 file 命令）。
# 2) dist/model-assets/ 存在时：manifest.json 逐文件 sha256/bytes 复核、
#    libonnxruntime.so AArch64 断言、资产合计 ≤ 350MiB（/tmp 512MB 默认
#    ephemeral 的文档化预算，需给查询 artifacts 留余量）。
# 用法：check-lambda-size-budget.sh [dist_dir]（默认 <repo>/dist）。
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly DIST_DIR="${1:-${LTSEARCH_DIST_DIR:-$REPO_ROOT/dist}}"

python3 - "$DIST_DIR" <<'PY'
import hashlib
import json
import struct
import sys
import zipfile
from pathlib import Path

FN_HARD_LIMIT = 250 * 1024 * 1024  # 单函数解压硬限
FN_WARN_LIMIT = 200 * 1024 * 1024
ASSET_TMP_BUDGET = 350 * 1024 * 1024  # /tmp 资产预算(512MB ephemeral 留余量)
FUNCTIONS = ("query_lambda", "write_lambda", "index_builder_lambda")

dist = Path(sys.argv[1])
failures = []
rows = []


def mib(value):
    return f"{value / 1048576:,.1f} MiB" if isinstance(value, int) else value


def elf_e_machine(header: bytes):
    if header[:4] != b"\x7fELF":
        return None
    return struct.unpack_from("<H", header, 0x12)[0]


def assert_aarch64(header: bytes, label: str) -> None:
    machine = elf_e_machine(header)
    if machine is None:
        failures.append(f"{label} is not an ELF binary")
    elif machine != 0xB7:  # EM_AARCH64
        failures.append(f"{label} e_machine=0x{machine:X}, expected 0xB7 (AArch64)")


for fn in FUNCTIONS:
    fn_zip = dist / f"{fn}.zip"
    if not fn_zip.exists():
        print(f"missing {fn_zip}; run scripts/package-lambda-zips.sh first", file=sys.stderr)
        sys.exit(1)
    with zipfile.ZipFile(fn_zip) as archive:
        unzipped = sum(info.file_size for info in archive.infolist())
        assert_aarch64(archive.open("bootstrap").read(20), f"{fn_zip.name}:bootstrap")
    rows.append((fn_zip.name, fn_zip.stat().st_size, unzipped))
    if unzipped > FN_HARD_LIMIT:
        failures.append(
            f"{fn}: unzipped {unzipped} bytes exceeds the 250MB Lambda "
            f"function limit ({FN_HARD_LIMIT}); is strip in effect?"
        )
    elif unzipped > FN_WARN_LIMIT:
        print(f"WARNING: {fn} unzipped {mib(unzipped)} — within 50MB of the 250MB limit")

print(f"{'artifact':<30}{'compressed':>14}{'unzipped':>14}{'headroom':>14}")
for name, compressed, unzipped in rows:
    print(f"{name:<30}{mib(compressed):>14}{mib(unzipped):>14}{mib(FN_HARD_LIMIT - unzipped):>14}")

assets_dir = dist / "model-assets"
manifest_path = assets_dir / "manifest.json"
if manifest_path.exists():
    manifest = json.loads(manifest_path.read_text())
    total = 0
    for entry in manifest["files"]:
        path = assets_dir / entry["name"]
        if not path.exists():
            failures.append(f"model asset missing: {entry['name']}")
            continue
        data = path.read_bytes()
        total += len(data)
        if len(data) != entry["bytes"]:
            failures.append(f"model asset {entry['name']}: {len(data)} bytes, manifest says {entry['bytes']}")
        if hashlib.sha256(data).hexdigest() != entry["sha256"]:
            failures.append(f"model asset {entry['name']}: sha256 mismatch vs manifest")
        if entry["name"] == "libonnxruntime.so":
            assert_aarch64(data[:20], "model-assets/libonnxruntime.so")
    print(f"{'model-assets (unzipped)':<30}{'-':>14}{mib(total):>14}{mib(ASSET_TMP_BUDGET - total):>14}")
    if total > ASSET_TMP_BUDGET:
        failures.append(
            f"model assets total {total} bytes exceeds the documented /tmp budget ({ASSET_TMP_BUDGET})"
        )
else:
    print("model-assets not staged; skipping asset hash/arch checks", file=sys.stderr)

if failures:
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    sys.exit(1)
print("size budget OK: functions fit the 250MB unzipped limit; assets verified")
PY
