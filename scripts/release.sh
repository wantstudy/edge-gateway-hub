#!/usr/bin/env bash
# =============================================================================
# release.sh —— 串起完整发版流程
#
# 顺序：check → build-windows → build-docker → check → 提示执行 publish.sh
#   （publish.sh 默认 dry-run，release.sh 不代替用户做发布决策，只做提示）
#
# 用法：
#   bash scripts/release.sh                      # 全量：门禁 → Windows → Docker → 门禁
#   bash scripts/release.sh --only check         # 只跑一致性门禁
#   bash scripts/release.sh --only windows       # 只构建 Windows 安装包
#   bash scripts/release.sh --only docker        # 只构建 Docker 镜像
#   bash scripts/release.sh --only windows --version 0.2.1
#   bash scripts/release.sh --skip-check-first   # 跳过开头门禁（构建产物已确认时用）
#
# 透传参数：--version / --skip-cargo / --no-frontend / --platform 会传给对应子脚本。
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

# --------------------------------------------------------------------------
# 参数
# --------------------------------------------------------------------------
ONLY=""
SKIP_CHECK_FIRST=0
PASS_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --only)             ONLY="${2:-}"; shift 2 ;;
    --only=*)           ONLY="${1#*=}"; shift ;;
    --skip-check-first) SKIP_CHECK_FIRST=1; shift ;;
    --version|--platform)
                        PASS_ARGS+=("$1" "${2:-}"); shift 2 ;;
    --version=*|--platform=*)
                        PASS_ARGS+=("$1"); shift ;;
    --skip-cargo|--no-frontend)
                        PASS_ARGS+=("$1"); shift ;;
    -h|--help)          sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)                  die "未知参数：$1（用 --help 查看用法）" ;;
  esac
done

case "${ONLY}" in
  ""|check|windows|docker|all) ;;
  *) die "--only 只支持：check / windows / docker / all（当前：${ONLY}）" ;;
esac
[[ -z "${ONLY}" ]] && ONLY="all"

want() { [[ "${ONLY}" == "all" || "${ONLY}" == "$1" ]]; }

require_iot_daq_src

# 从透传参数里取版本号；没给就回落到 tauri.conf.json
VER=""
HAS_VER_ARG=0
for ((i = 0; i < ${#PASS_ARGS[@]}; i++)); do
  if [[ "${PASS_ARGS[$i]}" == "--version" ]]; then
    VER="${PASS_ARGS[$((i + 1))]:-}"; HAS_VER_ARG=1
  fi
  if [[ "${PASS_ARGS[$i]}" == --version=* ]]; then
    VER="${PASS_ARGS[$i]#*=}"; HAS_VER_ARG=1
  fi
done
if [[ -z "${VER}" ]]; then VER="$(read_version)"; fi
valid_version_string "${VER}" || die "版本号格式非法：${VER}"

# 传给子脚本的参数：避免 --version 重复传两次（重复时后者覆盖前者，值相同但日志会重复解析）
CHILD_ARGS=()
if [[ "${HAS_VER_ARG}" == "0" ]]; then CHILD_ARGS+=(--version "${VER}"); fi
if [[ ${#PASS_ARGS[@]} -gt 0 ]]; then CHILD_ARGS+=("${PASS_ARGS[@]}"); fi

printf '\n==========================================================================\n'
printf ' release —— 发版流水线（版本 v%s，范围：%s）\n' "${VER}" "${ONLY}"
printf '==========================================================================\n'
log "分发仓   : ${REPO_ROOT}"
log "源码仓   : ${IOT_DAQ_SRC}"

START_TS="$(date +%s)"

# --------------------------------------------------------------------------
# 1) 前置门禁
# --------------------------------------------------------------------------
if want check && [[ "${SKIP_CHECK_FIRST}" == "0" ]]; then
  step 1 "前置一致性门禁"
  bash "${SCRIPT_DIR}/check-consistency.sh" --version "${VER}" || {
    printf '\n❌ 前置门禁未通过。构建前请先统一版本号/文件名引用。\n'
    exit 1
  }
elif want check; then
  step 1 "跳过前置门禁（--skip-check-first）"
fi

# --------------------------------------------------------------------------
# 2) Windows 安装包
# --------------------------------------------------------------------------
if want windows; then
  step 2 "构建 Windows 安装包"
  bash "${SCRIPT_DIR}/build-windows.sh" ${CHILD_ARGS[@]+"${CHILD_ARGS[@]}"}
else
  step 2 "跳过 Windows 构建（--only ${ONLY}）"
fi

# --------------------------------------------------------------------------
# 3) Docker 镜像
# --------------------------------------------------------------------------
if want docker; then
  step 3 "构建 Docker 镜像并打 Hub 双 tag"
  bash "${SCRIPT_DIR}/build-docker.sh" ${CHILD_ARGS[@]+"${CHILD_ARGS[@]}"}
else
  step 3 "跳过 Docker 构建（--only ${ONLY}）"
fi

# --------------------------------------------------------------------------
# 4) 收尾门禁
# --------------------------------------------------------------------------
if want check && [[ "${ONLY}" == "all" ]]; then
  step 4 "收尾一致性门禁（构建产物落位后复检）"
  bash "${SCRIPT_DIR}/check-consistency.sh" --version "${VER}" || {
    printf '\n❌ 收尾门禁未通过：构建产物与文档引用仍不一致。\n'
    exit 1
  }
fi

END_TS="$(date +%s)"
ELAPSED=$((END_TS - START_TS))

printf '\n==========================================================================\n'
printf ' release 流水线完成（耗时 %ss，范围：%s，版本 v%s）\n' "${ELAPSED}" "${ONLY}" "${VER}"
printf '==========================================================================\n'
printf '\n 下一步（发布到外部，默认 dry-run，请人工确认后再 --execute）：\n'
printf '   bash scripts/publish.sh --dry-run\n'
printf '   bash scripts/publish.sh --execute --website --git          # 推镜像 + 部署官网 + 推分发仓\n'
printf '   bash scripts/publish.sh --execute --skip-docker --git       # 只推官网与 git\n'
printf '   bash scripts/publish.sh --execute --server-load             # 服务器离线导入镜像\n'
printf '\n'
