#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# contract-runtime-cargo.sh —— 用 **Rust 测试套件的真实执行结果** 补上
# contract-runtime-server.sh 里只能 skip 的两条断言。
#
# 由来：契约 1（一机一码 403）与契约 5（档位默认 B）的关键判据，在
# licensing-server 内部是有**路由级集成测试**的（axum oneshot 真请求 → 真响应），
# 但它们跑在 `cargo test` 里，不打 HTTP 端口，所以我此前的 HTTP 脚本够不着，
# 只能登记 skip。本脚本把这两条 skip 换成真实执行证据。
#
# 纪律同 lib.sh：只认测试名后的 `ok` / `FAILED` 标记，不认「命令没报错」。
#
# 用法：
#   bash contract-runtime-cargo.sh [-p <package>] [-- <cargo test 额外参数...>]
# 退出码：0=全 PASS；1=有 FAIL；2=环境不满足（缺 cargo）。
# ---------------------------------------------------------------------------
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

C_TESTED_PKG="licensing-server"

while [ "$#" -gt 0 ]; do
    case "$1" in
        -p|--package) C_TESTED_PKG="${2:-licensing-server}"; shift 2 ;;
        --) shift; break ;;
        -h|--help) sed -n '2,26p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) printf '未知参数: %s\n' "$1" >&2; exit 2 ;;
    esac
done

section "实跑契约（cargo test）环境"

if ! command -v cargo >/dev/null 2>&1; then
    log "cargo 不在 PATH —— 本脚本依赖真实执行 Rust 测试套件，无法 skip 了事。"
    log "本项属「未验证」，请在有 Rust 工具链的环境补跑。"
    exit 2
fi
log "cargo: $(command -v cargo)"
cargo --version | sed 's/^/  /'

# --- 必须真实通过的契约断言（测试名 → 覆盖的契约条目）---
# 命名与 lib.sh 保持一致（C1 / C5）。
REQUIRED_TESTS=(
    "C1.7 同码异机（锚点 2/5）路由级实跑 403 CODE_BOUND_TO_OTHER_DEVICE"
    "http_activate_bound_to_other_device_returns_403_code_bound_to_other_device"
    "C1.8 异机错误载荷不含机器码/锚点等敏感值"
    "code_bound_to_other_device_carries_stable_kind_and_no_secret"
    "C5.5 建租户默认档位为 B（实跑）"
    "http_admin_tenant_create_then_issue_bootstrap_chain"
    "C5.6 业务字段不在白名单 → 被 FieldWhitelistViolation 拒收（实跑）"
    "whitelist_rejects_extra_business_field"
)

section "执行 cargo test -p ${C_TESTED_PKG}"
CARGO_OUT="$(mktemp)"
trap 'rm -f "${CARGO_OUT}"' EXIT
if ! cargo test -p "${C_TESTED_PKG}" >"${CARGO_OUT}" 2>&1; then
    printf '%sFAIL%s cargo test 自身未通过 —— 后续逐条断言无意义\n' "$C_RED" "$C_OFF"
    tail -40 "${CARGO_OUT}" | sed 's/^/        /'
    exit 1
fi
grep -E "^(running|test result)" "${CARGO_OUT}" | sed 's/^/  /'

# 逐个测试名断言其结果为 ok（不是「整包通过」就算过，逐条点名）。
# 断言名（NAME_*）与测试全名（TEST_*）平行排列；两者下标一一对应。
NAME_OF=(
    "C1.7 同码异机（锚点 2/5）路由级实跑 403 CODE_BOUND_TO_OTHER_DEVICE"
    "C1.8 异机错误载荷不含机器码/锚点等敏感值"
    "C5.5 建租户默认档位为 B（实跑）"
    "C5.6 业务字段不在白名单 → 被 FieldWhitelistViolation 拒收（实跑）"
)
TEST_OF=(
    "http_activate_bound_to_other_device_returns_403_code_bound_to_other_device"
    "code_bound_to_other_device_carries_stable_kind_and_no_secret"
    "http_admin_tenant_create_then_issue_bootstrap_chain"
    "whitelist_rejects_extra_business_field"
)
[ "${#NAME_OF[@]}" -eq "${#TEST_OF[@]}" ] || {
    printf '脚本自身缺陷：NAME_OF/TEST_OF 长度不一致\n' >&2; exit 2; }
get_result() { # get_result <test 全名> -> ok|FAILED|ignored|MISSING
    local t="$1" line
    line="$(grep -E "^test ${t} \.\.\. " "${CARGO_OUT}" | head -1 || true)"
    [ -z "$line" ] && { printf 'MISSING'; return; }
    case "$line" in
        *"... ok"*)          printf 'ok' ;;
        *"FAILED"*)          printf 'FAILED' ;;
        *"ignored"*)         printf 'ignored' ;;
        *)                   printf 'other' ;;
    esac
}

for i in "${!NAME_OF[@]}"; do
    spec="${NAME_OF[$i]}"
    res="$(get_result "${TEST_OF[$i]}")"
    case "$res" in
        ok) E_TOTAL=$((E_TOTAL+1)); E_PASS=$((E_PASS+1)); ok "${spec}" ;;
        MISSING)
            E_TOTAL=$((E_TOTAL+1)); E_FAIL=$((E_FAIL+1)); FAILED_NAMES+=("${spec}")
            bad "${spec} :: 测试未出现在 cargo 输出中（可能已被改名/删除）" ;;
        FAILED)
            E_TOTAL=$((E_TOTAL+1)); E_FAIL=$((E_FAIL+1)); FAILED_NAMES+=("${spec}")
            bad "${spec} :: 测试真实执行结果为 FAILED" ;;
        *)
            E_TOTAL=$((E_TOTAL+1)); E_FAIL=$((E_FAIL+1)); FAILED_NAMES+=("${spec}")
            bad "${spec} :: 测试结果为 '${res}'（须为 ok）" ;;
    esac
done

section "cargo test 输出中的契约留痕"
hint="$(grep -cE "^test .* \.\.\. ok$" "${CARGO_OUT}" || true)"
assert_true "TEST.1 本轮确有测试真实通过（${hint} 条 ok）" \
    "$( [ "${hint}" -gt 0 ] && echo 0 || echo 1 )"
assert_true "TEST.2 无测试为 ignored（ignored 不算覆盖）" \
    "$( ! grep -qE "^test .* \.\.\. ignored$" "${CARGO_OUT}" && echo 0 || echo 1 )" \
    "若出现 ignored，说明契约覆盖存在被 #[ignore] 绕过的空洞"

report "contract-runtime-cargo.sh"
