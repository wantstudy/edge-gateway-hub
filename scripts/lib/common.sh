#!/usr/bin/env bash
# =============================================================================
# lib/common.sh —— 发布脚本公共库
#
# 作用：被 scripts/ 下其它脚本 source，**不单独执行**。
# 提供：仓库路径解析、版本单一真源读取、常量、日志/断言工具、Rust 环境准备、
#       代理环境切换、exe 路径计算、Windows 路径转换。
#
# 运行环境：Windows Git Bash + 本机 Docker Desktop(WSL2) + 本机 Rust/Node。
# 设计原则：fail-closed（任何前置不满足立即非零退出，不做"尽力而为"）。
# =============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# 1. 路径解析
# ---------------------------------------------------------------------------
# 本文件位于 <分发仓>/scripts/lib/common.sh
_COMMON_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
SCRIPTS_DIR="$(cd -- "${_COMMON_DIR}/.." && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPTS_DIR}/.." && pwd -P)"   # 分发仓 edge-gateway-hub 根

DOWNLOADS_DIR="${REPO_ROOT}/downloads"
DOCKER_DIR="${REPO_ROOT}/docker"
WEBSITE_DIR="${REPO_ROOT}/website"
WEBSITE_INDEX="${WEBSITE_DIR}/index.html"            # 官网真实部署源（src/ 下 Vue SPA 是死代码）
README_FILE="${REPO_ROOT}/README.md"
COMPOSE_FILE="${DOCKER_DIR}/docker-compose.yml"
ENV_EXAMPLE_FILE="${DOCKER_DIR}/.env.example"

# iot-daq 源码仓：① 环境变量 IOT_DAQ_SRC ② 同级目录 ../iot-daq ③ 都没有 -> 空串
# 说明：这里不直接 die，因为 check-consistency.sh 需要允许"源码仓缺失、只校验分发仓"的场景；
#       真正需要源码仓的脚本调用 require_iot_daq_src() 来 fail-closed。
IOT_DAQ_SRC=""
resolve_iot_daq_src() {
  if [[ -n "${IOT_DAQ_SRC:-}" ]]; then
    if [[ ! -d "${IOT_DAQ_SRC}" ]]; then
      die "环境变量 IOT_DAQ_SRC 指向的目录不存在：${IOT_DAQ_SRC}"
    fi
    IOT_DAQ_SRC="$(cd -- "${IOT_DAQ_SRC}" && pwd -P)"
    return 0
  fi
  if [[ -d "${REPO_ROOT}/../iot-daq" ]]; then
    IOT_DAQ_SRC="$(cd -- "${REPO_ROOT}/../iot-daq" && pwd -P)"
    return 0
  fi
  IOT_DAQ_SRC=""
  return 0
}

# fail-closed 版本：源码仓不存在就报错退出
require_iot_daq_src() {
  resolve_iot_daq_src
  if [[ -z "${IOT_DAQ_SRC}" ]]; then
    die "未找到 iot-daq 源码仓。请二选一：
       1) export IOT_DAQ_SRC=/path/to/iot-daq
       2) 把 iot-daq 放在分发仓同级目录：${REPO_ROOT}/../iot-daq"
  fi
  log "源码仓 IOT_DAQ_SRC = ${IOT_DAQ_SRC}"
}

# ---------------------------------------------------------------------------
# 2. 常量
# ---------------------------------------------------------------------------
IMAGE_REPO="wantstudy/iot-daq-gateway"        # Docker Hub 仓库（必须带 wantstudy/ 前缀）
IMAGE_LATEST="${IMAGE_REPO}:latest"           # 滚动 latest 标签
CONTAINER_NAME="edge-gateway-hub"             # 容器名（README/官网/compose 三处必须一致）
LOCAL_IMAGE_PREFIX="iot-daq-gateway"          # 本机构建出来的本地镜像名（无前缀，仅本地用）

# exe 文件名模板：源码侧（Tauri/NSIS 产物）含空格；分发仓侧空格换成连字符
EXE_NAME_PATTERN_SRC="IoT-DAQ Gateway_%s_x64-setup.exe"
EXE_NAME_PATTERN_DST="IoT-DAQ-Gateway_%s_x64-setup.exe"

TAURI_CONF_REL="deploy/win/tauri-shell/src-tauri/tauri.conf.json"
TAURI_SHELL_REL="deploy/win/tauri-shell"
TAURI_TARGET_REL="deploy/win/tauri-shell/src-tauri/target"
BUNDLE_NSIS_REL="${TAURI_TARGET_REL}/release/bundle/nsis"

# 出网代理：本机靠它访问 Docker Hub / GitHub。Rust/npm 必须清掉，docker/git 必须保留。
LOCAL_PROXY="${LOCAL_PROXY:-http://127.0.0.1:60210}"

# 官网部署目标（可用环境变量覆盖）
SERVER_SSH_HOST="${SERVER_SSH_HOST:-root@60.205.8.146}"
SERVER_WEB_ROOT="${SERVER_WEB_ROOT:-/www/wwwroot/edge.webscad.cn/}"
GIT_SSH_KEY="${GIT_SSH_KEY:-${HOME}/.ssh/id_ed25519_github}"

# 官网里"看起来像产品版本号"但其实不是的 vX.Y.Z 标签（协议版本等），门禁时忽略。
# 例：官网写着 "MQTT v3.1.1 / v5"，v3.1.1 是 MQTT 协议版本，不是产品版本。
IGNORE_VERSION_TAGS="${IGNORE_VERSION_TAGS:-v3.1.1}"

# ---------------------------------------------------------------------------
# 3. 日志 / 断言 / 命令检查
# ---------------------------------------------------------------------------
log()  { printf '[INFO] %s\n' "$*"; }
step() { printf '\n[STEP %s] %s\n' "$1" "${2:-}"; }
ok()   { printf '  ✅ %s\n' "$*"; }
warn() { printf '  ⚠️  %s\n' "$*" >&2; }
die()  { printf '\n❌ %s\n' "$*" >&2; exit 1; }

# need_cmd <cmd> [cmd...] —— 缺失即 fail-closed
need_cmd() {
  local c
  for c in "$@"; do
    command -v "$c" >/dev/null 2>&1 || die "缺少必需命令：$c（请确认已安装并已加入 PATH）"
  done
}

# ---------------------------------------------------------------------------
# 4. 版本读取（单一真源 = tauri.conf.json 的 version 字段）
# ---------------------------------------------------------------------------
# 版本号一律按**字符串**处理，禁止数值比较。
read_version() {
  local conf="${IOT_DAQ_SRC}/${TAURI_CONF_REL}"
  [[ -f "${conf}" ]] || die "找不到 tauri.conf.json（版本单一真源）：${conf}"
  local ver=""
  ver="$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "${conf}" | head -n1)"
  [[ -n "${ver}" ]] || die "未能从 ${conf} 解析出 version 字段"
  printf '%s' "${ver}"
}

# 版本号基本形态校验（x.y.z，纯字符串匹配，不做数值比较）
valid_version_string() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
}

# ---------------------------------------------------------------------------
# 5. exe 路径（产物名含空格，调用方必须加引号）
# ---------------------------------------------------------------------------
exe_src_path() {   # 源码侧 NSIS 产物（含空格）
  local ver="$1"
  printf "${BUNDLE_NSIS_REL}/${EXE_NAME_PATTERN_SRC}" "${ver}" | sed "s|^|${IOT_DAQ_SRC}/|"
}
exe_dst_path() {   # 分发仓 downloads/ 下（空格 -> 连字符）
  local ver="$1"
  printf "${DOWNLOADS_DIR}/${EXE_NAME_PATTERN_DST}" "${ver}"
}

# ---------------------------------------------------------------------------
# 6. Rust 构建环境（清代理 + 固定工具链）
# ---------------------------------------------------------------------------
# 背景（踩过的坑）：
#   - 本机代理会让 cargo 拉取 crates.io 失败 -> cargo/npm 步骤必须先 unset 代理；
#   - rustup 自带 self-contained mingw 链接器会盖掉系统 mingw64，导致链接失败
#     -> /d/rust/mingw64/bin 必须排在 rustup 之前；
#   - docker push / git push 反过来必须**保留**代理（本机靠它出网），见 restore_proxy_env()。
_SAVED_HTTP_PROXY=""
_SAVED_HTTPS_PROXY=""
_SAVED_LOWER_HTTP_PROXY=""
_SAVED_LOWER_HTTPS_PROXY=""

setup_rust_env() {
  _SAVED_HTTP_PROXY="${HTTP_PROXY:-}"
  _SAVED_HTTPS_PROXY="${HTTPS_PROXY:-}"
  _SAVED_LOWER_HTTP_PROXY="${http_proxy:-}"
  _SAVED_LOWER_HTTPS_PROXY="${https_proxy:-}"

  unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy ALL_PROXY all_proxy

  export RUSTUP_HOME='D:/rust/rustup'
  export CARGO_HOME='D:/rust/cargo'
  export PATH="/d/rust/cargo/bin:/d/rust/mingw64/bin:${PATH}"

  log "Rust 环境已就绪（已清代理）：RUSTUP_HOME=${RUSTUP_HOME} CARGO_HOME=${CARGO_HOME}"
}

# 恢复代理：docker push / git push / scp 之前必须调用（与 setup_rust_env 配对使用）
restore_proxy_env() {
  if [[ -n "${_SAVED_HTTP_PROXY}" || -n "${_SAVED_HTTPS_PROXY}" ]]; then
    export HTTP_PROXY="${_SAVED_HTTP_PROXY}"
    export HTTPS_PROXY="${_SAVED_HTTPS_PROXY}"
    export http_proxy="${_SAVED_LOWER_HTTP_PROXY}"
    export https_proxy="${_SAVED_LOWER_HTTPS_PROXY}"
  else
    # 原始环境没有代理（例如单独执行本脚本），显式补上本机出网代理
    export HTTP_PROXY="${LOCAL_PROXY}"
    export HTTPS_PROXY="${LOCAL_PROXY}"
    export http_proxy="${LOCAL_PROXY}"
    export https_proxy="${LOCAL_PROXY}"
  fi
  unset ALL_PROXY all_proxy
  log "出网环境已恢复：HTTP_PROXY=${HTTP_PROXY}（docker push / git push 需要它）"
}

# ---------------------------------------------------------------------------
# 7. 安全删除 / 路径转换
# ---------------------------------------------------------------------------
# 本机安全删除机制会拦截并静默失败 rm -rf，因此一律用 mv 到系统 Temp。
TMP_ROOT="${TEMP:-${TMP:-/tmp}}"
TMP_ROOT="${TMP_ROOT//\\//}"      # Git Bash 里 TEMP 常带反斜杠，统一成正斜杠

trash_dir() {   # trash_dir <path> —— 把目录移入 Temp（不删除）
  local p="$1"
  [[ -e "${p}" ]] || return 0
  local dest="${TMP_ROOT}/iot-daq-trash-$(date +%Y%m%d-%H%M%S)-$$"
  mkdir -p "${TMP_ROOT}"
  log "清理（移动到 Temp，非 rm -rf）：${p} -> ${dest}"
  mv "${p}" "${dest}" || die "移动 ${p} 失败"
}

# POSIX 路径 /f/xy/... -> Windows 风格 F:/xy/...（给 docker 命令用）
to_win_path() {
  printf '%s' "$1" | sed -E 's|^/([a-zA-Z])/|\1:/|' | sed -E 's|^([a-z]):/|\u\1:/|'
}

# ---------------------------------------------------------------------------
# 8. 通用执行器（支持 dry-run：只打印不执行）
# ---------------------------------------------------------------------------
DRY_RUN="${DRY_RUN:-0}"
run_cmd() {   # run_cmd <描述> <命令...>
  local desc="$1"; shift
  printf '\n  ▶ %s\n' "${desc}"
  printf '    $'; printf ' %q' "$@"; printf '\n'
  if [[ "${DRY_RUN}" == "1" ]]; then
    printf '    [dry-run] 未执行\n'
    return 0
  fi
  "$@"
}

# ---------------------------------------------------------------------------
# 9. 从文档中提取镜像引用（README / 官网）
# ---------------------------------------------------------------------------
# 抽取 "docker run ..." 命令块里的镜像名：
# 从含 docker run 的行开始，遇到不以 \ 结尾的行结束；先剥 HTML 标签再取最后一个镜像引用。
extract_docker_run_image() {
  local file="$1"
  [[ -f "${file}" ]] || { printf ''; return 0; }
  awk '
    /docker run/ { inblk = 1 }
    inblk {
      line = $0
      gsub(/<[^>]*>/, "", line)
      print line
      if (line !~ /\\$/) { inblk = 0 }
    }
  ' "${file}" \
    | grep -oE '[A-Za-z0-9_./-]*iot-daq-gateway:[A-Za-z0-9._-]*' \
    | tail -n1 \
    | sed 's/[[:space:]]*$//' || true
}

extract_compose_image() {   # docker-compose.yml 的 image: 值
  local file="$1"
  [[ -f "${file}" ]] || { printf ''; return 0; }
  sed -n 's/^[[:space:]]*image:[[:space:]]*\([^[:space:]#]*\).*/\1/p' "${file}" | head -n1 || true
}

extract_env_image() {       # .env.example 的 IOT_DAQ_IMAGE 值
  local file="$1"
  [[ -f "${file}" ]] || { printf ''; return 0; }
  sed -n 's/^[[:space:]]*IOT_DAQ_IMAGE[[:space:]]*=[[:space:]]*\([^[:space:]#]*\).*/\1/p' "${file}" | head -n1 || true
}

# 从官网抽取"产品版本标签" vX.Y.Z（去重后按行输出）
# 过滤两类噪声：
#   1) 行上下文含 MQTT / proto / TLS / OPC / Modbus 等协议关键词的（如 "MQTT v3.1.1 / v5"）；
#   2) IGNORE_VERSION_TAGS 显式声明的标签。
extract_website_version_tags() {
  local file="$1"
  [[ -f "${file}" ]] || return 0
  local ignore_re
  ignore_re="$(printf '%s' "${IGNORE_VERSION_TAGS}" | tr ',' '|' | sed -E 's/[.]/\\./g')"
  grep -inE '\bv[0-9]+\.[0-9]+\.[0-9]+\b' "${file}" 2>/dev/null \
    | grep -viE 'mqtt|proto|tls|opc|modbus|schema\.org' \
    | grep -oiE '\bv[0-9]+\.[0-9]+\.[0-9]+\b' \
    | { grep -viE "^(${ignore_re})$" || true; } \
    | sort -u || true
}

# 从文档中抽取所有 exe 引用文件名（去重后按行输出）
extract_exe_refs() {
  local file="$1"
  [[ -f "${file}" ]] || return 0
  grep -oE 'IoT-DAQ[- ]Gateway_[0-9]+\.[0-9]+\.[0-9]+_x64-setup\.exe' "${file}" \
    | sort -u || true
}

# ---------------------------------------------------------------------------
# 10. 初始化（source 本文件时自动执行）
# ---------------------------------------------------------------------------
resolve_iot_daq_src
