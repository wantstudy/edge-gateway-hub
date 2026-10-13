#!/usr/bin/env bash
# =============================================================================
# build-windows.sh —— 本机一键构建 Windows 安装包（NSIS）并同步到 downloads/
#
# 链路（本机 mingw-gnu 工具链 + Node tauri CLI，没有 cargo tauri）：
#   1) 清 Tauri 构建缓存（mv 到 Temp，不用 rm -rf）
#   2) cargo build --release --bin iot-daq-daemon（产物输出到 src-tauri/target）
#   3) cp daemon.exe -> src-tauri/resources/iot-daq-daemon.exe（同名）
#   4) cd deploy/win/tauri-shell && npm run build（beforeBuildCommand 自动构建 web-console）
#   5) 取 NSIS 产物（文件名含空格）
#   6) 复制到分发仓 downloads/（空格 -> 连字符）
#   7) 自动跑 check-consistency.sh 的 exe + version 相关项
#
# 用法：
#   bash scripts/build-windows.sh
#   bash scripts/build-windows.sh --skip-cargo      # 跳过 cargo/npm，只做 5~7（重打包后重跑）
#                                                   # 注意：--skip-cargo 仍会跑 npm run build
#   bash scripts/build-windows.sh --version 0.2.1   # 覆盖版本（默认读 tauri.conf.json）
#   bash scripts/build-windows.sh --no-check        # 结束后不跑一致性门禁
#
# 注意：cargo/npm 阶段必须**清代理**（本机代理会让 crates.io 拉取失败）。
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

# --------------------------------------------------------------------------
# 参数
# --------------------------------------------------------------------------
SKIP_CARGO=0
NO_CHECK=0
VERSION_OVERRIDE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-cargo)   SKIP_CARGO=1; shift ;;
    --no-check)     NO_CHECK=1; shift ;;
    --version)      VERSION_OVERRIDE="${2:-}"; shift 2 ;;
    --version=*)    VERSION_OVERRIDE="${1#*=}"; shift ;;
    -h|--help)      sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)              die "未知参数：$1（用 --help 查看用法）" ;;
  esac
done

require_iot_daq_src

if [[ -n "${VERSION_OVERRIDE}" ]]; then
  VER="${VERSION_OVERRIDE}"
  valid_version_string "${VER}" || die "--version 格式非法：${VER}"
else
  VER="$(read_version)"
fi

SRC_EXE="$(exe_src_path "${VER}")"
DST_EXE="$(exe_dst_path "${VER}")"
TAURI_BUILD_CACHE="${IOT_DAQ_SRC}/${TAURI_TARGET_REL}/release/build"
RESOURCES_DIR="${IOT_DAQ_SRC}/deploy/win/tauri-shell/src-tauri/resources"
DAEMON_BIN="${IOT_DAQ_SRC}/${TAURI_TARGET_REL}/release/iot-daq-daemon.exe"

printf '\n==========================================================================\n'
printf ' build-windows —— 构建 Windows 安装包 v%s\n' "${VER}"
printf '==========================================================================\n'
log "源码仓     : ${IOT_DAQ_SRC}"
log "NSIS 产物  : ${SRC_EXE}"
log "分发仓落点 : ${DST_EXE}"
if [[ "${SKIP_CARGO}" == "1" ]]; then
  warn "--skip-cargo：跳过 cargo 编译与缓存清理（沿用已有 target/），仍会重跑 npm run build"
fi

need_cmd cargo npm node

# --------------------------------------------------------------------------
# 1) 清 Tauri 构建缓存 + 准备 Rust 环境（清代理）
# --------------------------------------------------------------------------
if [[ "${SKIP_CARGO}" == "0" ]]; then
  step 1 "准备 Rust 环境（清空代理）并清理 Tauri 构建缓存"
  setup_rust_env
  # 已知坑：缓存里残留旧绝对路径，会让 tauri-build 读取 plugin permissions 失败
  trash_dir "${TAURI_BUILD_CACHE}"
  ok "缓存已清理"
else
  step 1 "跳过 cargo 阶段（--skip-cargo），仅准备环境变量"
  setup_rust_env
fi

# --------------------------------------------------------------------------
# 2) 编译 daemon 侧车
# --------------------------------------------------------------------------
if [[ "${SKIP_CARGO}" == "0" ]]; then
  step 2 "编译 daemon 侧车（耗时较长）"
  printf '  $ CARGO_TARGET_DIR=%s cargo build --release --bin iot-daq-daemon\n' \
    "${IOT_DAQ_SRC}/${TAURI_TARGET_REL}"
  ( cd "${IOT_DAQ_SRC}" \
    && CARGO_TARGET_DIR="${IOT_DAQ_SRC}/${TAURI_TARGET_REL}" \
       cargo build --release --bin iot-daq-daemon )
  [[ -f "${DAEMON_BIN}" ]] || die "daemon 产物缺失：${DAEMON_BIN}"
  ok "daemon 已生成：${DAEMON_BIN}"

  # ------------------------------------------------------------------------
  # 3) 拷贝到 src-tauri/resources/（Tauri 会把它作为 sidecar 打进安装包）
  # ------------------------------------------------------------------------
  step 3 "拷贝 daemon 到 src-tauri/resources/"
  mkdir -p "${RESOURCES_DIR}"
  cp -f "${DAEMON_BIN}" "${RESOURCES_DIR}/iot-daq-daemon.exe"
  ok "已就位：${RESOURCES_DIR}/iot-daq-daemon.exe"
else
  step 2 "跳过 daemon 编译（--skip-cargo）"
  step 3 "跳过 resources 同步（--skip-cargo）"
  [[ -f "${RESOURCES_DIR}/iot-daq-daemon.exe" ]] \
    || die "resources 下没有 iot-daq-daemon.exe，无法跳过 cargo 阶段：${RESOURCES_DIR}"
fi

# --------------------------------------------------------------------------
# 4) npm run build（= tauri build；Node tauri CLI，本机没有 cargo tauri）
# --------------------------------------------------------------------------
step 4 "npm run build（tauri build，自动构建 web-console 后出 NSIS 包）"
# npm 前必须清代理（setup_rust_env 已 unset，这里再兜底一次，防止被上游脚本覆盖）
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
( cd "${IOT_DAQ_SRC}/${TAURI_SHELL_REL}" && npm run build )
ok "tauri build 完成"

# --------------------------------------------------------------------------
# 5) 取 NSIS 产物（文件名含空格，必须全程加引号）
# --------------------------------------------------------------------------
step 5 "确认 NSIS 产物"
if [[ ! -f "${SRC_EXE}" ]]; then
  printf '\n  当前 bundle/nsis 目录下的实际文件：\n'
  ls -l "${IOT_DAQ_SRC}/${BUNDLE_NSIS_REL}" 2>/dev/null || true
  die "找不到 NSIS 产物：\"${SRC_EXE}\"
       若版本号不匹配，可用 --version <实际版本> 覆盖；bundle targets 只有 nsis（无 msi）。"
fi
ok "产物存在：\"${SRC_EXE}\"（$(wc -c < "${SRC_EXE}" | tr -d ' ') 字节）"

# --------------------------------------------------------------------------
# 6) 同步到分发仓 downloads/（空格 -> 连字符）
# --------------------------------------------------------------------------
step 6 "同步到分发仓 downloads/（空格替换为连字符）"
mkdir -p "${DOWNLOADS_DIR}"
cp -f "${SRC_EXE}" "${DST_EXE}"
ok "已发布到分发仓：${DST_EXE}"

# --------------------------------------------------------------------------
# 7) 自动跑一致性门禁（exe + version 相关项）
# --------------------------------------------------------------------------
if [[ "${NO_CHECK}" == "1" ]]; then
  step 7 "跳过一致性门禁（--no-check）"
else
  step 7 "运行一致性门禁（exe + version 分组）"
  bash "${SCRIPT_DIR}/check-consistency.sh" --version "${VER}" --only exe,version || true
fi

printf '\n✅ build-windows 完成：v%s\n' "${VER}"
printf '   下一步：bash scripts/build-docker.sh（构建并打 Hub 双 tag）\n'
