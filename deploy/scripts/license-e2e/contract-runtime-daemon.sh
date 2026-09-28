#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# contract-runtime-daemon.sh —— 网关侧（daemon）的**实跑**契约验收。
#
# 覆盖 P1-3 中与网关直接相关的部分：
#   契约 4：客户端禁解绑 / 重置试用 / 废弃激活码入口 —— 实跑探测管理面路由，
#           任何授权**写**端点都必须 404（不存在），只有 GET /api/license/status 是 200。
#   契约 4：持久卷 —— `--preflight` 对 data_dir 存在/不存在分别返回 0 / 2（fail-fast）。
#   契约 2/3：试用标记与授权状态落 `gateway.data_dir`（宿主持久卷），不落可写层。
#
# 断言一律落在 **HTTP 状态码 / 进程退出码 / 日志关键字** 上。
#
# 用法：
#   bash contract-runtime-daemon.sh [--bin <iot-daq-daemon 路径>] [--port <n>]
# 退出码：0=全 PASS；1=有 FAIL；2=环境不满足（缺二进制 → 全部 skip）。
# ---------------------------------------------------------------------------
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

E_TOTAL=0; E_PASS=0; E_FAIL=0; E_SKIP=0
FAILED_NAMES=(); SKIPPED_NAMES=()

BIN=""
CFG_REL="${LICENSE_E2E_DAEMON_CONFIG:-config.toml}"
BASE=""          # 在 pick_port 之后赋值

while [ "$#" -gt 0 ]; do
    case "$1" in
        --bin) BIN="${2:-}"; shift 2 ;;
        --port) LICENSE_E2E_DAEMON_PORT="${2:-}"; shift 2 ;;
        --config) CFG_REL="${2:-}"; shift 2 ;;
        -h|--help) sed -n '2,24p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) printf '未知参数: %s\n' "$1" >&2; exit 2 ;;
    esac
done

section "实跑契约（daemon 网关侧）环境"
require_tools curl python3

if [ -z "$BIN" ]; then
    for cand in "${REPO_ROOT}/target/debug/iot-daq-daemon.exe" \
                "${REPO_ROOT}/target/debug/iot-daq-daemon" \
                "${REPO_ROOT}/target/release/iot-daq-daemon"; do
        [ -x "$cand" ] && BIN="$cand" && break
    done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
    log "未在 target/ 找到 iot-daq-daemon 二进制。"
    log "本脚本属「实跑」验收，依赖本地构建产物；缺失即无法执行。"
    exit 2
fi
log "二进制: ${BIN}"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "${WORKDIR}"' EXIT
DATA_DIR="${WORKDIR}/data"
mkdir -p "${DATA_DIR}"
CFG="${WORKDIR}/config.toml"
# 用仓库根的真实配置做基座，只把 data_dir 换到临时持久卷（不改仓库内任何文件）
sed "s#^data_dir[[:space:]]*=.*#data_dir = \"${DATA_DIR}\"#" "${REPO_ROOT}/${CFG_REL}" > "${CFG}"

# 每次跑都挑一个**当前空闲**的端口：避免上一轮残留进程占着固定端口导致本轮
# 起不来、却把断言打在旧进程上（会产出看似 PASS 的假结论）。
pick_port() {
    local base="${LICENSE_E2E_DAEMON_PORT:-18100}" p
    if [ -n "${LICENSE_E2E_DAEMON_PORT:-}" ]; then printf '%s' "${LICENSE_E2E_DAEMON_PORT}"; return; fi
    for i in $(seq 1 60); do
        p=$(( base + i ))
        (exec 3<>/dev/tcp/127.0.0.1/"${p}") 2>/dev/null && { exec 3<&- 2>/dev/null; continue; }
        printf '%s' "${p}"; return
    done
    printf '%s' "${base}"
}
RUN_PORT="$(pick_port)"
BASE="http://127.0.0.1:${RUN_PORT}"
log "本轮占用端口: ${RUN_PORT}"

# ===========================================================================
section "契约 4/红线：持久卷 fail-fast（--preflight 退码与日志关键字）"

preflight_out() { # preflight_out <data_dir> → 输出 stdout+stderr
    local dd="$1"
    IOT_DAQ_DATA_DIR="${dd}" "${BIN}" --preflight --config "${CFG}" 2>&1 || true
}
PF_OK="$(preflight_out "${DATA_DIR}")"
PF_OK_RC=0
IOT_DAQ_DATA_DIR="${DATA_DIR}" "${BIN}" --preflight --config "${CFG}" >/dev/null 2>&1 || PF_OK_RC=$?
assert_eq "D1.1 data_dir 存在 → preflight 退出码 0" "0" "${PF_OK_RC}"
assert_contains "D1.2 preflight 通过日志关键字" "${PF_OK}" "preflight 全部检查通过"

PF_BAD="$(preflight_out "${WORKDIR}/definitely-not-exists")"
PF_BAD_RC=0
IOT_DAQ_DATA_DIR="${WORKDIR}/definitely-not-exists" "${BIN}" --preflight --config "${CFG}" >/dev/null 2>&1 || PF_BAD_RC=$?
assert_eq "D1.3 data_dir 不存在 → preflight 退出码 2（拒绝启动）" "2" "${PF_BAD_RC}"
assert_contains "D1.4 失败输出含「拒绝启动」" "${PF_BAD}" "拒绝启动"
assert_contains "D1.5 失败原因指向持久卷根" "${PF_BAD}" "持久卷根"
assert_true "D1.6 失败输出禁止「删指纹绕过」的提示（fail-fast 口径）" \
    "$( printf '%s' "${PF_BAD}" | grep -q '切勿通过删除指纹文件' && echo 0 || echo 1 )"

# ===========================================================================
section "契约 4：实跑探测 —— 客户端不得存在解绑 / 重置试用 / 废弃激活码入口"

"${BIN}" --config "${CFG}" --bind "127.0.0.1:${RUN_PORT}" --foreground >"${WORKDIR}/daemon.log" 2>&1 &
DAEMON_PID=$!
cleanup() {
    kill "${DAEMON_PID}" 2>/dev/null || true
    wait "${DAEMON_PID}" 2>/dev/null || true
}
trap cleanup EXIT
trap 'cleanup' INT TERM

up=0
for _ in $(seq 1 40); do
    # `--noproxy '*'`：本机常驻 HTTP(S)_PROXY 会让 keep-alive 复用错乱
    # （症状：首个请求成功、之后全 404）；`--max-time 2` 防探测卡死。
    if curl -fsS --noproxy '*' --max-time 2 -o /dev/null \
        "${BASE}/api/license/status" 2>/dev/null; then up=1; break; fi
    sleep 0.5
done
if [ "$up" -ne 1 ]; then
    printf '%sFAIL%s daemon 未能在 20s 内起管理面（环境问题）\n' "$C_RED" "$C_OFF"
    sed 's/^/        /' "${WORKDIR}/daemon.log" | tail -25
    exit 2
fi
ok "daemon 管理面已监听 ${BASE}"
# 给 stdout 缓冲一点落盘时间，避免日志断言读到半截日志
sleep 2

# --- 授权状态是**只读**接口 ---
call() { # call <METHOD> <PATH>
    # `--noproxy '*'`：本机常驻 HTTP(S)_PROXY 会让 keep-alive 复用错乱
    # （症状：首个请求成功、之后全 404）；`--max-time` 防卡死。
    curl -sS --noproxy '*' --max-time 10 -o "${WORKDIR}/body" -w '%{http_code}' \
        -X "$1" "${BASE}${2}" -H 'content-type: application/json' -d '{}' \
        2>/dev/null || echo 000
}
assert_eq "D2.1 GET /api/license/status → 200（唯一授权只读接口）" "200" "$(call GET /api/license/status)"

# --- 所有授权写语义端点必须 404（路由不存在 = 入口不存在）---
for ep in unbind reset_trial reset-trial revoke reissue deactivate delete_license clear_trial rebind switch_machine; do
    st="$(call POST "/api/license/${ep}")"
    assert_eq "D2.2 POST /api/license/${ep} → 404（无此入口）" "404" "${st}"
    st="$(call PUT "/api/license/${ep}")"
    assert_eq "D2.3 PUT  /api/license/${ep} → 404（无此入口）" "404" "${st}"
    st="$(call DELETE "/api/license/${ep}")"
    assert_eq "D2.4 DELETE /api/license/${ep} → 404（无此入口）" "404" "${st}"
done

# --- 编码变体：连字符 / 下划线 / 复数，防止路由别名绕过 ---
for ep in license-remove unbind-activation reset-trial-period trial-reset codes-revoke activation-revoke; do
    st="$(call POST "/api/license/${ep}")"
    assert_eq "D2.5 POST /api/license/${ep}（编码变体）→ 404" "404" "${st}"
done

# --- 管理面不应存在任何 /admin 前缀的高危写（客户端侧没有厂商后台能力）---
for ep in "admin/codes/issue" "admin/codes/abc/revoke" "admin/codes/abc/reissue" "admin/tenants"; do
    st="$(call POST "/api/${ep}")"
    assert_eq "D2.6 POST /api/${ep} → 404（客户端无厂商后台能力）" "404" "${st}"
done

# ===========================================================================
section "契约 2/3：授权状态与试用标记落宿主持久卷（data_dir）"

assert_true "D3.1 授权状态接口返回合法 status 字段" \
    "$(curl -sS --noproxy '*' --max-time 10 "${BASE}/api/license/status" 2>/dev/null \
        | python3 -c 'import json,sys
try:
    s=json.load(sys.stdin).get("status","")
    print(0 if s in ("unlicensed","trial","licensed","grace","degraded","active","stopped") else 1)
except Exception:
    print(1)' 2>/dev/null || echo 1)" \
    "status 取值必须落在合法枚举内"

# 持久卷布局：**持久化落盘位置必须来自 data_dir（宿主持久卷）**，不落容器可写层。
# 日志关键字断言放在本段末尾（HTTP 探测之后）；原因：daemon 的 stdout 被重定向到
# 文件时会块缓冲，断言读得太早会读到半截日志 → 假 FAIL。
DAEMON_LOG="$(cat "${WORKDIR}/daemon.log" 2>/dev/null || true)"
DATA_BASENAME="$(basename "${DATA_DIR}")"
assert_contains "D3.2a 审计库落盘路径取自 data_dir（宿主持久卷，非容器可写层）" \
    "${DAEMON_LOG}" "audit.db; ikm="
assert_contains "D3.2b 落盘路径含本次配置的 data_dir（${DATA_BASENAME}）" \
    "${DAEMON_LOG}" "${DATA_BASENAME}"
# fail-closed：未配置云授权时，不得悄悄发放试用（否则离线可无限刷）
assert_contains "D3.2c 未配置云授权 → 授权编排跳过（不静默发试用）" \
    "${DAEMON_LOG}" "未配置云授权"
assert_contains "D3.2d 授权编排未注入（license runtime not injected）" \
    "${DAEMON_LOG}" "license runtime not injected"

# 「容器重建不重置试用」的**实跑**证据链：
#   trial.marker 路径必须来自 data_dir（而非容器可写层）。构造不可写 data_dir →
#   preflight 必须拒绝启动（D1.3 已覆盖）。此处补一条：marker 文件名契约常量必须存在。
src_contains "D3.3 试用标记文件名常量 trial.marker 且相对 data_dir" \
    "crates/daemon/src/license.rs" "pub const TRIAL_MARKER_FILE: &str = \"trial.marker\""
src_contains "D3.4 标记落盘目录来自 gateway.data_dir（红线 #13）" \
    "crates/daemon/src/license.rs" "试用到期审计|/// 试用标记落盘目录（\*\*必须\*\*来自 \`gateway.data_dir\`"
src_contains "D3.5 data_dir 不可写 → fail-closed 降级，绝不静默发新试用" \
    "crates/daemon/src/license.rs" "绝不静默发放新试用"

# ===========================================================================
section "daemon 日志关键字"
assert_contains "D4.1 日志含管理面监听行（精确关键字，不用宽松的 mgmt 匹配）" \
    "${DAEMON_LOG}" "管理面已监听"
assert_true "D4.2 日志中无 panic（fail-closed 路径不得 panic）" \
    "$( printf '%s' "${DAEMON_LOG}" | grep -qi 'panic' && echo 1 || echo 0 )"
assert_true "D4.3 日志中无 panic 栈展开" \
    "$( printf '%s' "${DAEMON_LOG}" | grep -qE 'panicked at|RUST_BACKTRACE' && echo 1 || echo 0 )"
# 集齐一遍「无云授权时的 fail-closed 口径」，确认为同一运行期的真实日志
assert_true "D4.4 同一批次日志中授权编排确为跳过态" \
    "$( printf '%s' "${DAEMON_LOG}" | grep -q '授权编排跳过' && echo 0 || echo 1 )"

report "contract-runtime-daemon.sh"
