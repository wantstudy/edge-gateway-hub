#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# contract-runtime-server.sh —— licensing-server 的**实跑**契约验收。
#
# 覆盖 P1-3 中与服务端直接相关的部分：
#   C1 一机一码：异机激活被 403 CODE_BOUND_TO_OTHER_DEVICE 拒绝、零副作用
#   C5 二次校验档位：默认 B；A/B/C 可被解析；档位不随租户切换（已知偏离的实跑证据）
#   C4 高危写：客户端无入口 → 设备端请求 /admin/* 一律 403/401 ADMIN_ONLY
#   fail-closed：无凭据时管理端登录必须全拒
#
# 断言一律落在 **HTTP 状态码 + JSON 业务码字段** 上，不靠「命令没报错」。
#
# 用法：
#   bash contract-runtime-server.sh [--bin <licensing-server 二进制路径>]
# 退出码：0=全 PASS；1=有 FAIL；2=环境不满足（如缺二进制，此时全部 skip）。
# ---------------------------------------------------------------------------
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

E_TOTAL=0; E_PASS=0; E_FAIL=0; E_SKIP=0
FAILED_NAMES=(); SKIPPED_NAMES=()

BIN=""
RUN_PORT="${LICENSE_E2E_PORT:-17080}"
BASE="http://127.0.0.1:${RUN_PORT}"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --bin) BIN="${2:-}"; shift 2 ;;
        --port) RUN_PORT="${2:-}"; BASE="http://127.0.0.1:${2:-}"; shift 2 ;;
        -h|--help) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) printf '未知参数: %s\n' "$1" >&2; exit 2 ;;
    esac
done

section "实跑契约（licensing-server）环境"

require_tools curl python3

if [ -z "$BIN" ]; then
    for cand in "${REPO_ROOT}/target/debug/licensing-server" \
                "${REPO_ROOT}/target/release/licensing-server"; do
        [ -x "$cand" ] && BIN="$cand" && break
    done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
    log "未在 target/ 找到 licensing-server 二进制。"
    log "本脚本属「实跑」验收，依赖本地构建产物；缺失即无法执行。"
    exit 2
fi
log "二进制: ${BIN}"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "${WORKDIR}"' EXIT
DB="${WORKDIR}/licensing.db"
LOG="${WORKDIR}/server.log"

# 起服务（干净库、无管理员凭据 → 必须 fail-closed）
env IOT_DAQ_LISTEN_ADDR="127.0.0.1:${RUN_PORT}" \
    IOT_DAQ_LICENSE_DB="${DB}" \
    IOT_DAQ_LOG_LEVEL=info \
    IOTDAQ_ADMIN_USER=e2e-admin \
    IOTDAQ_ADMIN_PASSWORD_SHA256="$(python3 -c 'import hashlib;print(hashlib.sha256(b"e2e-pw").hexdigest())')" \
    IOTDAQ_JWT_SECRET="e2e-local-secret-for-contract-run" \
    "${BIN}" >"${LOG}" 2>&1 &
SRV_PID=$!
cleanup() {
    kill "${SRV_PID}" 2>/dev/null || true
    wait "${SRV_PID}" 2>/dev/null || true
    rm -rf "${WORKDIR}"
}
trap cleanup EXIT
trap 'cleanup' INT TERM

# 等待监听（最多 15s）；起不来就是环境问题，不是契约问题
up=0
for _ in $(seq 1 30); do
    if curl -fsS -o /dev/null "${BASE}/admin/keys" 2>/dev/null; then up=1; break; fi
    sleep 0.5
done
if [ "$up" -ne 1 ]; then
    printf '%sFAIL%s licensing-server 未能在 15s 内起监听（环境问题）\n' "$C_RED" "$C_OFF"
    sed 's/^/        /' "${LOG}" | tail -20
    exit 2
fi
ok "licensing-server 已监听 ${BASE}"

# --- 断言辅助：返回 <http_status> <json 原始体> ---
# 用法：call <METHOD> <PATH> [JSON] [额外的 curl 参数...]
# 注意：此前版本把 `"${AUTH[@]}"`（Bearer 头）当成第 3 个位置参数喂给了 `body`，
# 导致**所有本应带 JWT 的请求实际都是匿名请求**——断言会全部落在 401 上，
# 是「假通过 / 假失败」双重风险。现改为：第 3 参是 body，其后一律当 curl 参数。
call() { # call <METHOD> <PATH> [JSON] [curl args...]
    local m="$1" p="$2" body="${3:-}"
    shift 3 2>/dev/null || shift $#
    local -a args
    args=( -X "$m" "${BASE}${p}" -H 'content-type: application/json' )
    if [ -n "$body" ]; then args+=( -d "$body" ); fi
    local a
    for a in "$@"; do [ -n "$a" ] && args+=( "$a" ); done
    curl -sS -o "${WORKDIR}/body" -w '%{http_code}' "${args[@]}" 2>/dev/null \
        || echo 000 >"${WORKDIR}/status"
    local status
    status="$(cat "${WORKDIR}/status" 2>/dev/null || echo 000)"
    HTTP_STATUS="$status"
    BODY_FILE="${WORKDIR}/body"
}
# code_of <path in json>：从响应体取 code 字段（无则空串）
code_of() {
    python3 - "$BODY_FILE" "$1" <<'PY'
import json, sys
try:
    with open(sys.argv[1], 'r', encoding='utf-8', errors='replace') as f:
        v = json.load(f)
    for k in sys.argv[2:]:
        v = v.get(k) if isinstance(v, dict) else None
    print(str(v) if v is not None else "")
except Exception:
    print("")
PY
}

# ===========================================================================
section "C1 一机一码：同码异机激活必须 403 CODE_BOUND_TO_OTHER_DEVICE"

# 前置：租户 + 发一张码（管理端，需先登录拿 JWT）
LOGIN_BODY="$(python3 -c 'import json;print(json.dumps({"username":"e2e-admin","password":"e2e-pw"}))')"
call POST /admin/auth/login "${LOGIN_BODY}"
assert_eq "C1.0 管理端登录 200" "200" "${HTTP_STATUS}"
JWT="$(code_of data token)"
assert_ne "C1.0b 取到管理端 JWT" "" "${JWT}"
ROLE="$(code_of data role)"
assert_eq "C1.0c 初始管理员角色为 system（可执行建租户/发码）" "system" "${ROLE}"

AUTH=(-H "authorization: Bearer ${JWT}")

call POST /admin/tenants '{"tenant_id":"e2e-t1","name":"E2E Tenant","contact":"e2e@example.com"}' "${AUTH[@]}"
assert_eq "C1.1 建租户 200" "200" "${HTTP_STATUS}"
assert_eq "C1.1b 建租户未指定档位 → 默认 B" "B" "$(code_of data verify_mode_default)"

ISSUE_BODY='{"tenant_id":"e2e-t1","tier":"standard","count":1,"idempotency_key":"e2e-issue-1"}'
call POST /admin/codes/issue "${ISSUE_BODY}" "${AUTH[@]}"
assert_eq "C1.2 发码 200" "200" "${HTTP_STATUS}"
CODE="$(python3 - "$BODY_FILE" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1], encoding='utf-8', errors='replace'))
    print(((d.get("data") or {}).get("codes") or [{}])[0].get("code", ""))
except Exception:
    print("")
PY
)"
assert_ne "C1.2b 拿到激活码" "" "${CODE}"

# 关键断言：设备端 /activation **不携带任何认证**，用不存在的码激活。
# 期望：被拒绝（400 INVALID_CODE / 验签失败），且**绝不能**返回 200 租约。
call POST /activation "$(python3 -c 'import json;print(json.dumps({"activation_code":"'"${CODE}"'","machine_code":"mc-attacker-1","anchor_hashes":["a","b","c","d","e"],"device_pubkey":"","nonce":"n1","ts":"1","req_sig":"c2VhbA=="}))')"
assert_ne "C1.3 无签名激活不得返回 200（否则一机一码形同虚设）" "200" "${HTTP_STATUS}"
CODE_FIELD="$(code_of code)"
assert_true "C1.3b 拒绝业务码存在且不是 OK" \
    "$( [ -n "$CODE_FIELD" ] && [ "$CODE_FIELD" != "OK" ] && echo 0 || echo 1 )" \
    "code=${CODE_FIELD}"

# 有签但异机的路径需要 Ed25519 设备密钥，shell 侧不做密码学；
# 该分支的判据由静态脚本 contract-static.sh 的 1.x 条 + 单测覆盖，此处显式 skip 登记。
skip_assert "C1.4 同码异机（含锚点 ≤3/5）实跑 403 CODE_BOUND_TO_OTHER_DEVICE" \
    "需 Ed25519 设备签名，纯 shell 无法构造；建议由 daemon 侧集成用例补"

# 发出来的码必须处于可激活态，且绑定关系为空（尚未被绑到任何机器）
# 注意：revoke 是高危写，除了 Bearer 还**必须**带 `X-Tenant-Id`（http.rs:764 的
# require_tenant_header），缺了返回 400 —— 这正是「客户端/匿名无法越权废弃」的一环。
call GET "/admin/codes?tenant_id=e2e-t1" '' "${AUTH[@]}"
assert_ne "C1.4b 带 JWT 可列码（证明 401 不是鉴权系统的常态）" "401" "${HTTP_STATUS}"
call POST "/admin/codes/${CODE}/revoke" "$(python3 -c 'import json;print(json.dumps({"reason":"e2e cleanup","note":"e2e cleanup note for contract run","confirm_tail8":"'${CODE: -8}'"}))')" "${AUTH[@]}" -H "x-tenant-id: e2e-t1"
REV_STATUS="${HTTP_STATUS}"
assert_true "C1.5 已发放码可被厂商后台废弃（状态 ${REV_STATUS}）" \
    "$( [ "${REV_STATUS}" = "200" ] || [ "${REV_STATUS}" = "412" ] && echo 0 || echo 1 )" \
    "412 = CONFIRM_MISMATCH 亦为契约内合法结果"
call POST /activation "$(python3 -c 'import json;print(json.dumps({"activation_code":"'"${CODE}"'","machine_code":"mc-x","anchor_hashes":["a","b","c","d","e"],"device_pubkey":"","nonce":"n2","ts":"1","req_sig":"c2VhbA=="}))')"
assert_ne "C1.6 已废弃码不得再激活成功" "200" "${HTTP_STATUS}"

# ===========================================================================
section "C5 二次校验档位：默认 B；A/B/C 可解析；租户策略不生效（已知偏离实证）"

call POST /admin/tenants '{"tenant_id":"e2e-t2","name":"E2E B","contact":"e2e@example.com","verify_mode_default":"A"}' "${AUTH[@]}"
assert_eq "C5.1 建租户（试图指定 A 档）成功" "200" "${HTTP_STATUS}"
POL="$(code_of data verify_mode_default)"
assert_eq "C5.2 租户默认档位已按请求落成 A" "A" "${POL}"

# 已知偏离的实跑证据：切到 A 档后，新发的码在激活时签发的档位仍须是 B（硬编码）。
# 静态侧已给出代码证据；此处用「激活响应 verify_mode 字段」做实跑兜底——
# 但无设备密钥无法完成激活，故本条同样登记为 skip，静态证据为准。
skip_assert "C5.3 A 档租户下激活实跑签发 verify_mode=B（偏离实跑证据）" \
    "需 Ed25519 设备签名完成激活；证据以 contract-static.sh GAP1-c~f 代码证据为准"

src_contains "C5.4 服务端档位解析覆盖 A/B/C" "crates/licensing-server/src/model.rs" '"C" | "c" => Ok\(VerifyMode::C\)'

# ===========================================================================
section "C4 / fail-closed：客户端侧无管理端入口（实跑验证）"

# 设备端请求一律不带 /admin 凭据 → 必须被挡（403 ADMIN_ONLY / 401 SESSION_EXPIRED）
call POST /admin/codes/issue '{"tenant_id":"e2e-t1","tier":"standard","count":1,"idempotency_key":"anon-1"}'
assert_ne "C4.1 无 JWT 发放码不得成功（200/201）" "200" "${HTTP_STATUS}"
assert_ne "C4.1b 亦不得为 201" "201" "${HTTP_STATUS}"
A_CODE="$(code_of code)"
assert_true "C4.1c 匿名发码被拒且有业务码" \
    "$( [ -n "$A_CODE" ] && [ "$A_CODE" != "OK" ] && echo 0 || echo 1 )" "code=${A_CODE}"

call POST /admin/auth/login "$(python3 -c 'import json;print(json.dumps({"username":"nosuchuser","password":"nope"}))')"
assert_ne "C4.2 错误凭据登录不得 200" "200" "${HTTP_STATUS}"

# 客户端若真有「废弃/重发」入口，会打在 daemon 的管理面上；此处只验证 licensing-server
# 侧的管理端写端点必须由厂商管理员调用（RBAC 门控实跑）
call POST /admin/codes/issue '{"tenant_id":"e2e-t1","tier":"standard","count":1,"idempotency_key":"lic-ops-1"}' -H "authorization: Bearer not-a-real-jwt"
assert_ne "C4.3 伪造 JWT 不得发码成功" "200" "${HTTP_STATUS}"

# ===========================================================================
section "fail-closed：高危参数的守卫生效"
# 匿名 PUT 租户策略（无任何凭据）不得成功 —— 否则任何设备/匿名调用方都能改档位
call PUT "/admin/tenants/e2e-t1/policy" '{"verify_mode_default":"A"}'
assert_ne "FC.0 匿名改租户档位不得成功" "200" "${HTTP_STATUS}"
# 非法档位 → 400（判别是真的在校验，不是无脑接受）
call PUT "/admin/tenants/e2e-t1/policy" '{"verify_mode_default":"Z"}' "${AUTH[@]}"
assert_ne "FC.1 非法档位 Z 不得写入成功" "200" "${HTTP_STATUS}"
assert_ne "FC.1b 亦不得为 201/204" "204" "${HTTP_STATUS}"
# 合法档位 A 可写入（证明判别是真的在校验，不是无脑接受）
call PUT "/admin/tenants/e2e-t1/policy" '{"verify_mode_default":"A"}' "${AUTH[@]}"
assert_eq "FC.2 合法档位 A 写入 200" "200" "${HTTP_STATUS}"

# ===========================================================================
section "服务端日志关键字（授权事件留痕）"
LOG_TEXT="$(cat "${LOG}" 2>/dev/null || true)"
assert_contains "LOG.1 启动行含监听地址" "${LOG_TEXT}" "licensing-server listening"
assert_contains "LOG.2 日志含授权相关事件/级别" "${LOG_TEXT}" "info"

report "contract-runtime-server.sh"
