# 本地 HTTP 黑盒 E2E 公共库（#141）：生命周期、端口发现、请求/响应记录、
# 版本轮询与失败诊断。供 run-local-real-flow.sh 及后续 #142/#143 契约套件复用。
#
# 隔离模型：每次运行独立 compose project（-p <prefix>-<run_id>）+ 临时 host 端口
# （compose 文件写 "127.0.0.1::8080"，实际端口经 `docker compose port` 发现）+
# project 前缀派生的卷/网络（compose 文件不得写死 name:）。并发运行互不冲突。
#
# 清理语义（对齐 #147：teardown 前收集、teardown 总是执行）：
#   - 无论成败，`down -v --remove-orphans` 都会执行；
#   - 失败时先 dump 诊断（compose ps + 各服务 logs + 已记录的请求/响应载荷），
#     并保留 run dir（.e2e-tmp/<prefix>-<run_id>/）供排查；
#   - 成功时连 run dir 一并删除。
#
# 用法（bash 3.2 兼容，调用方需 set -euo pipefail）：
#   source "$REPO_ROOT/scripts/e2e/local_http_lib.sh"
#   lhttp_init "$REPO_ROOT/docker-compose.local-ltembed.yml" ltsearch-real
#   trap 'lhttp_finish $?' EXIT
#   lhttp_up
#   WRITE_BASE="http://127.0.0.1:$(lhttp_port write)"
#   lhttp_request health-write GET "$WRITE_BASE/health"
#   ...

# 初始化本次运行：设置 LHTTP_COMPOSE_FILE / LHTTP_PROJECT / LHTTP_RUN_DIR。
# $1=compose 文件绝对路径 $2=project 前缀 [$3=overlay compose 文件绝对路径]
# overlay（如 docker-compose.local-ltembed.degraded.yml）与 base 按 compose
# 合并语义叠加，仅供改写 env 等局部差异，拓扑/隔离语义仍由 base 承担。
lhttp_init() {
  LHTTP_COMPOSE_FILE="$1"
  local prefix="$2"
  LHTTP_COMPOSE_OVERLAY="${3:-}"
  local run_id="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
  LHTTP_PROJECT="$prefix-$run_id"
  local repo_root
  repo_root="$(cd "$(dirname "$LHTTP_COMPOSE_FILE")" && pwd)"
  LHTTP_RUN_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$repo_root/.e2e-tmp}/$LHTTP_PROJECT"
  mkdir -p "$LHTTP_RUN_DIR"
  echo "run dir: $LHTTP_RUN_DIR (project: $LHTTP_PROJECT)" >&2
}

lhttp_compose() {
  if [ -n "${LHTTP_COMPOSE_OVERLAY:-}" ]; then
    docker compose -p "$LHTTP_PROJECT" -f "$LHTTP_COMPOSE_FILE" \
      -f "$LHTTP_COMPOSE_OVERLAY" "$@"
  else
    docker compose -p "$LHTTP_PROJECT" -f "$LHTTP_COMPOSE_FILE" "$@"
  fi
}

lhttp_up() {
  lhttp_compose up -d --wait
}

# no-wait 启动：供刻意降级的拓扑使用——degraded overlay 下 query/build 的
# healthcheck 注定 unhealthy，`up -d --wait` 必然失败，须以本函数启动后用
# lhttp_wait_http_ready 等 HTTP 可达，再对 /health 断言具体状态码（含 503）。
lhttp_up_nowait() {
  lhttp_compose up -d
}

lhttp_down() {
  lhttp_compose down -v --remove-orphans
}

# 保留共享卷重启整个拓扑（重启持久性用例）：down 刻意不带 -v——卷就是被验证
# 的持久层，清了卷"持久性"就成假绿。容器重建后三角色的临时 host 端口全部
# 变化，调用方必须重新经 lhttp_port 发现端口再发请求。
lhttp_restart() {
  lhttp_compose down --remove-orphans
  lhttp_up
}

# 主动快照当前容器日志：$1=标签。down/up 重建会销毁旧容器及其日志，重启类
# 用例须在 lhttp_restart 前调用本函数，失败诊断才看得到重启前各阶段的日志。
lhttp_snapshot_logs() {
  local label="$1" service
  for service in $(lhttp_compose config --services 2>/dev/null); do
    lhttp_compose logs --no-color "$service" \
      > "$LHTTP_RUN_DIR/$label-$service.log" 2>&1 || true
  done
}

# 发现服务的临时 host 端口：$1=service [$2=容器端口，默认 8080]
# 空结果即失败：容器未起（或 down/up 重建后未重新发现）时立刻报根因，
# 不让空端口拼进 URL 变成难懂的 curl 错误。
lhttp_port() {
  local service="$1" cport="${2:-8080}"
  local port
  port=$(lhttp_compose port "$service" "$cport" | awk -F: 'NF { print $NF; exit }')
  if [ -z "$port" ]; then
    echo "lhttp_port: no host port for service '$service' (container not up? ports change after every up/recreate and must be rediscovered)" >&2
    return 1
  fi
  echo "$port"
}

# 发起 HTTP 请求并全量落盘请求/响应（AC-5 载荷记录）。
# $1=记录名（文件名前缀） $2=METHOD $3=URL [$4=请求体文件]
# 响应体写入 $LHTTP_RUN_DIR/<名>.response.json 并回显到 stdout；
# HTTP 状态码写入全局 LHTTP_STATUS（curl 传输失败时置 000 并返回非零）。
lhttp_request() {
  local name="$1" method="$2" url="$3" body_file="${4:-}"
  local out="$LHTTP_RUN_DIR/$name.response.json"
  {
    echo "$method $url"
    if [ -n "$body_file" ]; then cat "$body_file"; fi
  } > "$LHTTP_RUN_DIR/$name.request.txt"
  local curl_rc=0
  # 单次请求必须有界（默认 600s 兜底，LHTTP_CURL_MAX_TIME 可按场景收紧）：
  # 端口 accept 但服务不响应时无界 curl 会永久挂起，上层轮询的超时预算
  # 永远轮不到检查，最终挂到外层 job timeout 而不是有序诊断/清理。
  # -D 落盘响应头（诊断用，如 405 的 Allow）；契约断言仍只读状态码与 JSON body。
  if [ -n "$body_file" ]; then
    LHTTP_STATUS=$(curl -s -o "$out" -D "$LHTTP_RUN_DIR/$name.headers.txt" \
      -w '%{http_code}' --max-time "${LHTTP_CURL_MAX_TIME:-600}" -X "$method" \
      -H 'Content-Type: application/json' -d @"$body_file" "$url") || curl_rc=$?
  else
    LHTTP_STATUS=$(curl -s -o "$out" -D "$LHTTP_RUN_DIR/$name.headers.txt" \
      -w '%{http_code}' --max-time "${LHTTP_CURL_MAX_TIME:-600}" \
      -X "$method" "$url") || curl_rc=$?
  fi
  if [ "$curl_rc" -ne 0 ]; then
    LHTTP_STATUS=000
    echo "curl transport failure ($name): $method $url" >&2
    return "$curl_rc"
  fi
  echo "$LHTTP_STATUS" > "$LHTTP_RUN_DIR/$name.status"
  cat "$out"
  echo >&2
}

# 断言上一次 lhttp_request 的状态码：$1=期望码 $2=记录名（报错用）
lhttp_assert_status() {
  local expected="$1" name="$2"
  if [ "$LHTTP_STATUS" != "$expected" ]; then
    echo "$name: expected HTTP $expected, got $LHTTP_STATUS" >&2
    return 1
  fi
}

# 请求 + 状态码断言合体：$1=记录名 $2=METHOD $3=URL $4=期望码 [$5=请求体文件]。
# 供大量 404/405/400 契约用例使用；期望非 2xx 时 curl 传输失败（000）也会被
# 断言兜住而不是让 set -e 提前退出。
lhttp_expect_status() {
  local name="$1" method="$2" url="$3" expected="$4" body_file="${5:-}"
  if [ -n "$body_file" ]; then
    lhttp_request "$name" "$method" "$url" "$body_file" >/dev/null || true
  else
    lhttp_request "$name" "$method" "$url" >/dev/null || true
  fi
  lhttp_assert_status "$expected" "$name"
}

# 断言上一次响应是扁平错误信封：$1=记录名 $2=期望 error_type [$3=message 子串]。
# 必要键 {error_type, message} 必须在顶层存在（扁平，无嵌套 error 对象）；
# 只断必要键不锁全集,允许未来新增 request id 等向后兼容字段。404/405 的响应
# 是 axum 默认的空 body，不得走本函数——那两类只能用 lhttp_expect_status
# 断状态码。
lhttp_assert_error_body() {
  local name="$1" expected_type="$2" msg_substring="${3:-}"
  python3 - "$LHTTP_RUN_DIR/$name.response.json" "$expected_type" "$msg_substring" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert {"error_type", "message"} <= set(body), body
assert body["error_type"] == sys.argv[2], body
if sys.argv[3]:
    assert sys.argv[3] in body["message"], body
PY
}

# 断言角色健康：$1=记录名 $2=base URL [$3=期望 component]。期望 200（query/build
# 的 /health 内含真实 embedding probe，200 即真实推理健康——#141 AC-4 的实测点）。
# 三角色同镜像 + 临时端口，端口错配是最真实的翻车方式；传入期望 component
#（ltsearch-write / ltsearch-index-builder / ltsearch-query）可一并锁定。
lhttp_assert_health() {
  local name="$1" base="$2" component="${3:-}"
  lhttp_request "$name" GET "$base/health" >/dev/null
  lhttp_assert_status 200 "$name"
  if [ -n "$component" ]; then
    python3 - "$LHTTP_RUN_DIR/$name.response.json" "$component" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
assert body.get("component") == sys.argv[2], (sys.argv[2], body)
PY
  fi
}

# 等待 URL 的 HTTP 层可达：$1=记录名 $2=URL [$3=超时秒，默认 60]。
# 只等「任意非 000 状态」（TCP/HTTP 打通），不等特定状态码——供 no-wait 启动的
# 降级拓扑用：可达后由调用方对 /health 断言 503/200，若降级没生效是快失败
# 而不是把超时耗光。每轮经 lhttp_request 落盘（同名覆写），保留最后一次载荷。
# 超时契约：单次请求经 LHTTP_CURL_MAX_TIME 收紧到 5s（LHTTP_READY_MAX_TIME
# 可覆盖），总预算用 $SECONDS 墙钟核算——只 accept 不响应的对端也会在
# timeout_s 内失败返回，而不是挂在第一次 curl 上。
lhttp_wait_http_ready() {
  local name="$1" url="$2" timeout_s="${3:-60}"
  local start="$SECONDS"
  while :; do
    LHTTP_CURL_MAX_TIME="${LHTTP_READY_MAX_TIME:-5}" \
      lhttp_request "$name" GET "$url" >/dev/null 2>&1 || true
    [ "${LHTTP_STATUS:-000}" != "000" ] && return 0
    [ $((SECONDS - start)) -ge "$timeout_s" ] && break
    sleep 2
  done
  echo "$name: $url not HTTP-reachable within ${timeout_s}s" >&2
  return 1
}

# 轮询 query /health 直到 index_version >= $2（默认上限 180s）。
# $1=query base URL $2=目标版本 [$3=超时秒]。版本写入全局 LHTTP_VERSION。
# 每轮经 lhttp_request 落盘（同名覆写），超时失败时 run dir 里保留最后一次
# 轮询的请求/响应/状态码（AC-5 载荷记录覆盖轮询阶段）。
lhttp_wait_index_version() {
  local base="$1" target="$2" timeout_s="${3:-180}"
  local waited=0
  LHTTP_VERSION=0
  while :; do
    lhttp_request poll-index-version GET "$base/health" >/dev/null 2>&1 || true
    if [ "${LHTTP_STATUS:-000}" = "200" ]; then
      LHTTP_VERSION=$(python3 -c 'import json,sys;print(json.load(sys.stdin).get("index_version") or 0)' \
        < "$LHTTP_RUN_DIR/poll-index-version.response.json" 2>/dev/null || echo 0)
    fi
    [ "$LHTTP_VERSION" -ge "$target" ] && return 0
    [ "$waited" -ge "$timeout_s" ] && break
    sleep 2
    waited=$((waited + 2))
  done
  echo "index version never reached $target within ${timeout_s}s (last seen: $LHTTP_VERSION, last status: ${LHTTP_STATUS:-000})" >&2
  return 1
}

# 失败诊断：compose ps + 各服务日志落盘 run dir 并输出 stderr，附已记录载荷清单。
lhttp_dump_diagnostics() {
  echo "=== diagnostics for $LHTTP_PROJECT (kept in $LHTTP_RUN_DIR) ===" >&2
  lhttp_compose ps > "$LHTTP_RUN_DIR/compose-ps.txt" 2>&1 || true
  cat "$LHTTP_RUN_DIR/compose-ps.txt" >&2 || true
  local service
  for service in $(lhttp_compose config --services 2>/dev/null); do
    lhttp_compose logs --no-color "$service" > "$LHTTP_RUN_DIR/$service.log" 2>&1 || true
    echo "--- $service logs (tail) ---" >&2
    tail -40 "$LHTTP_RUN_DIR/$service.log" >&2 || true
  done
  echo "--- recorded request/response payloads ---" >&2
  local f
  for f in "$LHTTP_RUN_DIR"/*.request.txt; do
    [ -e "$f" ] || continue
    echo "### $f" >&2
    cat "$f" >&2
    local resp="${f%.request.txt}.response.json"
    if [ -e "$resp" ]; then
      echo "### $resp (status: $(cat "${f%.request.txt}.status" 2>/dev/null || echo '?'))" >&2
      cat "$resp" >&2
      echo >&2
    fi
  done
}

# EXIT trap 入口：$1=脚本退出码。teardown 总是执行；失败保留诊断，成功清空资源。
# teardown 本身失败不得被吞（AC"成功时清理全部测试资源"）：成功运行遇 down
# 失败时转为失败退出并保留诊断，调用方/CI 能察觉资源泄漏。
lhttp_finish() {
  local exit_code="$1"
  trap - EXIT
  if [ "$exit_code" -ne 0 ]; then
    lhttp_dump_diagnostics || true
  fi
  local down_rc=0
  lhttp_down || down_rc=$?
  if [ "$down_rc" -ne 0 ]; then
    echo "teardown failed (rc=$down_rc): project $LHTTP_PROJECT may have leaked containers/volumes/networks" >&2
    if [ "$exit_code" -eq 0 ]; then
      lhttp_dump_diagnostics || true
      exit_code="$down_rc"
    fi
  fi
  if [ "$exit_code" -eq 0 ]; then
    rm -rf "$LHTTP_RUN_DIR"
  else
    echo "diagnostics preserved: $LHTTP_RUN_DIR" >&2
  fi
  return "$exit_code"
}
