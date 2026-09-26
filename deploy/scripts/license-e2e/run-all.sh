#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# run-all.sh —— P1-3「授权端到端」验收总入口。
#
# 依次执行：
#   1) contract-static.sh         静态契约核查（只读，不需构建）
#   2) contract-runtime-daemon.sh 网关侧实跑（需 iot-daq-daemon 二进制）
#   3) contract-runtime-server.sh 服务端实跑（需 licensing-server 二进制）
#
# 语义：
#   * 任一环节 FAIL → 总退出码非 0（fail-closed，绝不「只要没崩就过」）。
#   * 缺二进制的环节记为 SKIP 并**明确打印**，不静默通过。
#   * 已知偏离需要显式 acknowledge 才算 PASS（见 contract-static.sh 顶部）。
#
# 用法： bash run-all.sh [--static-only]
# ---------------------------------------------------------------------------
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

STATIC="${SCRIPT_DIR}/contract-static.sh"
DAEMON_RT="${SCRIPT_DIR}/contract-runtime-daemon.sh"
SERVER_RT="${SCRIPT_DIR}/contract-runtime-server.sh"

STATIC_ONLY=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --static-only) STATIC_ONLY=1; shift ;;
        -h|--help) sed -n '2,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) printf '未知参数: %s\n' "$1" >&2; exit 2 ;;
    esac
done

printf '%s\n' "=================================================================="
printf '%s\n' " P1-3 授权端到端验收（一机一码 / 换机重发 / 容器重建不重置试用）"
printf '%s\n' "=================================================================="

FAILED=0
run_stage() { # run_stage <脚本名> <扇出模式?>
    local script="$1" rc=0
    printf '\n%s▶ %s%s\n' $'\033[1m' "$script" $'\033[0m'
    set +e
    bash "$script" 2>&1 | tee /tmp/license-e2e-$(basename "$script").log
    rc="${PIPESTATUS[0]}"
    set -e
    case "$rc" in
        0) printf '  >> %s: PASS (exit 0)\n' "$script" ;;
        1) printf '  >> %s: FAIL (exit 1)\n' "$script"; FAILED=1 ;;
        2) printf '  >> %s: 环境不满足/全部 skip (exit 2)\n' "$script" ;;
        *) printf '  >> %s: 异常退出码 %d\n' "$script" "$rc"; FAILED=1 ;;
    esac
    return 0
}

if [ ! -x "${STATIC}" ]; then chmod +x "${STATIC}" 2>/dev/null || true; fi
run_stage "${STATIC}"

if [ "${STATIC_ONLY}" -eq 0 ]; then
    [ -x "${DAEMON_RT}" ]  || chmod +x "${DAEMON_RT}" 2>/dev/null || true
    [ -x "${SERVER_RT}" ]  || chmod +x "${SERVER_RT}" 2>/dev/null || true
    run_stage "${DAEMON_RT}"
    run_stage "${SERVER_RT}"
fi

printf '\n==================================================================\n'
if [ "${FAILED}" -ne 0 ]; then
    printf '总结论: %sFAIL%s —— 存在未通过的契约断言，禁止据此放行。\n' $'\033[31m' $'\033[0m'
    exit 1
fi
printf '总结论: PASS（注：exit 2 的环节 = 环境不满足、已全部 skip，需人工补跑）\n'
