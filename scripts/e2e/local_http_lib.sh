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
# $1=compose 文件绝对路径 $2=project 前缀
lhttp_init() {
  LHTTP_COMPOSE_FILE="$1"
  local prefix="$2"
  local run_id="${LTSEARCH_E2E_RUN_ID:-$(date +%s)-$$}"
  LHTTP_PROJECT="$prefix-$run_id"
  local repo_root
  repo_root="$(cd "$(dirname "$LHTTP_COMPOSE_FILE")" && pwd)"
  LHTTP_RUN_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$repo_root/.e2e-tmp}/$LHTTP_PROJECT"
  mkdir -p "$LHTTP_RUN_DIR"
  echo "run dir: $LHTTP_RUN_DIR (project: $LHTTP_PROJECT)" >&2
}

lhttp_compose() {
  docker compose -p "$LHTTP_PROJECT" -f "$LHTTP_COMPOSE_FILE" "$@"
}

lhttp_up() {
  lhttp_compose up -d --wait
}

lhttp_down() {
  lhttp_compose down -v --remove-orphans
}

# 发现服务的临时 host 端口：$1=service [$2=容器端口，默认 8080]
lhttp_port() {
  local service="$1" cport="${2:-8080}"
  lhttp_compose port "$service" "$cport" | awk -F: 'NF { print $NF; exit }'
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
  if [ -n "$body_file" ]; then
    LHTTP_STATUS=$(curl -s -o "$out" -w '%{http_code}' -X "$method" \
      -H 'Content-Type: application/json' -d @"$body_file" "$url") || curl_rc=$?
  else
    LHTTP_STATUS=$(curl -s -o "$out" -w '%{http_code}' -X "$method" "$url") || curl_rc=$?
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

# 断言角色健康：$1=记录名 $2=base URL。期望 200（query/build 的 /health 内含
# 真实 embedding probe，200 即真实推理健康——#141 AC-4 的实测点）。
lhttp_assert_health() {
  local name="$1" base="$2"
  lhttp_request "$name" GET "$base/health" >/dev/null
  lhttp_assert_status 200 "$name"
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
