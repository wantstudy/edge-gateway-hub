#!/usr/bin/env bash
# =============================================================================
# check-consistency.sh —— 发布一致性门禁（fail-closed）
#
# 用途：在"构建之后 / 发布之前"校验分发仓里所有对版本号、exe 文件名、镜像名的
#       引用是否互相一致。任何一项不一致 -> 非零退出。
#
# 校验项：
#   ① downloads/IoT-DAQ-Gateway_<VER>_x64-setup.exe 存在
#   ② website/index.html 引用的 exe 文件名 == 实际文件名
#   ③ README.md 引用的 exe 文件名 == 实际文件名
#   ④ 镜像引用四处一致：官网 docker run == README docker run == compose image == .env IOT_DAQ_IMAGE
#   ⑤ 全仓搜索 iot-daq-gateway: 但缺少 wantstudy/ 前缀的引用（README 真实踩过的 bug）
#   ⑥ iot-daq 侧 Cargo.toml workspace version == tauri.conf.json version（源码仓存在才查）
#   ⑦ 官网 v<VER> 版本标签与 tauri 版本一致
#
# 用法：
#   bash scripts/check-consistency.sh
#   bash scripts/check-consistency.sh --version 0.2.0        # 覆盖版本（源码仓可缺失）
#   bash scripts/check-consistency.sh --only exe,image       # 只跑部分组
#      分组：exe(=①②③) / image(=④) / prefix(=⑤) / version(=⑥⑦) / all（默认）
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

# --------------------------------------------------------------------------
# 参数
# --------------------------------------------------------------------------
VERSION_OVERRIDE=""
ONLY_GROUPS="all"

usage() {
  sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION_OVERRIDE="${2:-}"; shift 2 ;;
    --version=*) VERSION_OVERRIDE="${1#*=}"; shift ;;
    --only) ONLY_GROUPS="${2:-}"; shift 2 ;;
    --only=*) ONLY_GROUPS="${1#*=}"; shift ;;
    -h|--help) usage ;;
    *) die "未知参数：$1（用 --help 查看用法）" ;;
  esac
done

group_enabled() {
  local g="$1"
  [[ "${ONLY_GROUPS}" == "all" ]] && return 0
  [[ ",${ONLY_GROUPS}," == *",${g},"* ]] && return 0
  return 1
}
any_group_enabled() {
  local g
  for g in "$@"; do group_enabled "${g}" && return 0; done
  return 1
}

# --------------------------------------------------------------------------
# 结果记录
# --------------------------------------------------------------------------
declare -a RES_ID=() RES_NAME=() RES_STATUS=() RES_DETAIL=()
N_PASS=0
N_FAIL=0

record() {  # record <id> <name> <PASS|FAIL|SKIP> <detail>
  RES_ID+=("$1"); RES_NAME+=("$2"); RES_STATUS+=("$3"); RES_DETAIL+=("$4")
  case "$3" in
    PASS) N_PASS=$((N_PASS + 1)) ;;
    FAIL) N_FAIL=$((N_FAIL + 1)) ;;
  esac
  local mark="✅ PASS"
  [[ "$3" == "FAIL" ]] && mark="❌ FAIL"
  [[ "$3" == "SKIP" ]] && mark="⏭️  SKIP"
  printf '  %-4s %-34s %s\n' "$1" "$2" "${mark}"
  [[ -n "$4" ]] && printf '       %s\n' "$4"
  return 0
}

# --------------------------------------------------------------------------
# 版本解析
# --------------------------------------------------------------------------
if [[ -n "${VERSION_OVERRIDE}" ]]; then
  VER="${VERSION_OVERRIDE}"
  valid_version_string "${VER}" || die "--version 传入的版本号格式非法：${VER}（应为 x.y.z 字符串）"
  log "版本（命令行覆盖）：${VER}"
else
  require_iot_daq_src
  VER="$(read_version)"
  log "版本（tauri.conf.json 单一真源）：${VER}"
fi

EXE_EXPECT_BASENAME="$(printf "${EXE_NAME_PATTERN_DST}" "${VER}")"
EXE_EXPECT_PATH="${DOWNLOADS_DIR}/${EXE_EXPECT_BASENAME}"
EXPECT_IMAGE_VER="${IMAGE_REPO}:${VER}"

printf '\n==========================================================================\n'
printf ' 一致性门禁 check-consistency —— 版本 %s（分组：%s）\n' "${VER}" "${ONLY_GROUPS}"
printf '==========================================================================\n'

# --------------------------------------------------------------------------
# ① downloads 下 exe 存在
# --------------------------------------------------------------------------
if group_enabled exe; then
  step 1 "downloads/ 下存在版本对应的 Windows 安装包"
  if [[ -f "${EXE_EXPECT_PATH}" ]]; then
    local_size="$(wc -c < "${EXE_EXPECT_PATH}" | tr -d ' ')"
    record "①" "downloads exe 存在" PASS "${EXE_EXPECT_BASENAME}（${local_size} 字节）"
  else
    record "①" "downloads exe 存在" FAIL "缺失：${EXE_EXPECT_PATH}
       请先跑 scripts/build-windows.sh，或确认版本号 ${VER} 是否正确。"
  fi

  # ------------------------------------------------------------------------
  # ② 官网 index.html 引用的 exe 文件名 == 实际文件名
  # ------------------------------------------------------------------------
  step 2 "官网 website/index.html 引用的 exe 文件名一致"
  if [[ ! -f "${WEBSITE_INDEX}" ]]; then
    record "②" "官网 exe 引用一致" FAIL "找不到 ${WEBSITE_INDEX}"
  else
    mapfile -t web_exe_refs < <(extract_exe_refs "${WEBSITE_INDEX}")
    if [[ ${#web_exe_refs[@]} -eq 0 ]]; then
      record "②" "官网 exe 引用一致" FAIL "${WEBSITE_INDEX} 里没有找到任何 exe 文件名引用"
    else
      bad_web=0
      for r in "${web_exe_refs[@]}"; do
        [[ "${r}" == "${EXE_EXPECT_BASENAME}" ]] || bad_web=1
      done
      if [[ ${bad_web} -eq 0 ]]; then
        record "②" "官网 exe 引用一致" PASS "引用 ${#web_exe_refs[@]} 处，均为 ${EXE_EXPECT_BASENAME}"
      else
        record "②" "官网 exe 引用一致" FAIL "官网引用：$(printf '%s ' "${web_exe_refs[@]}")
       期望值：${EXE_EXPECT_BASENAME}"
      fi
    fi
  fi

  # ------------------------------------------------------------------------
  # ③ README.md 引用的 exe 文件名 == 实际文件名
  # ------------------------------------------------------------------------
  step 3 "README.md 引用的 exe 文件名一致"
  if [[ ! -f "${README_FILE}" ]]; then
    record "③" "README exe 引用一致" FAIL "找不到 ${README_FILE}"
  else
    mapfile -t rd_exe_refs < <(extract_exe_refs "${README_FILE}")
    if [[ ${#rd_exe_refs[@]} -eq 0 ]]; then
      record "③" "README exe 引用一致" FAIL "${README_FILE} 里没有找到任何 exe 文件名引用"
    else
      bad_rd=0
      for r in "${rd_exe_refs[@]}"; do
        [[ "${r}" == "${EXE_EXPECT_BASENAME}" ]] || bad_rd=1
      done
      if [[ ${bad_rd} -eq 0 ]]; then
        record "③" "README exe 引用一致" PASS "引用 ${#rd_exe_refs[@]} 处，均为 ${EXE_EXPECT_BASENAME}"
      else
        record "③" "README exe 引用一致" FAIL "README 引用：$(printf '%s ' "${rd_exe_refs[@]}")
       期望值：${EXE_EXPECT_BASENAME}"
      fi
    fi
  fi
fi

# --------------------------------------------------------------------------
# ④ 镜像引用四处一致
# --------------------------------------------------------------------------
if group_enabled image; then
  step 4 "镜像引用四处一致（官网 / README / compose / .env）"
  web_img="$(extract_docker_run_image "${WEBSITE_INDEX}")"
  rd_img="$(extract_docker_run_image "${README_FILE}")"
  compose_img="$(extract_compose_image "${COMPOSE_FILE}")"
  env_img="$(extract_env_image "${ENV_EXAMPLE_FILE}")"

  printf '       官网   docker run : %s\n' "${web_img:-<未解析>}"
  printf '       README docker run : %s\n' "${rd_img:-<未解析>}"
  printf '       compose image     : %s\n' "${compose_img:-<未解析>}"
  printf '       .env IOT_DAQ_IMAGE: %s\n' "${env_img:-<未解析>}"

  # 收集所有非空值，去重后应只剩 1 个
  uniq_count="$(printf '%s\n' "${web_img}" "${rd_img}" "${compose_img}" "${env_img}" \
    | grep -v '^$' | sort -u | wc -l | tr -d ' ')"
  if [[ -z "${web_img}" || -z "${rd_img}" || -z "${compose_img}" || -z "${env_img}" ]]; then
    record "④" "镜像引用四处一致" FAIL "至少有一处未能解析出镜像引用（见上方四行）"
  elif [[ "${uniq_count}" == "1" ]]; then
    record "④" "镜像引用四处一致" PASS "四处均为 ${web_img}"
  else
    record "④" "镜像引用四处一致" FAIL "四处出现 ${uniq_count} 种不同取值：
       $(printf '%s\n' "${web_img}" "${rd_img}" "${compose_img}" "${env_img}" | grep -v '^$' | sort -u | tr '\n' ' ')
       建议统一为 ${IMAGE_LATEST}（官网/compose）或 ${EXPECT_IMAGE_VER}（README/发行说明）"
  fi
fi

# --------------------------------------------------------------------------
# ⑤ 缺少 wantstudy/ 前缀的镜像引用
# --------------------------------------------------------------------------
if group_enabled prefix; then
  step 5 "全仓扫描：iot-daq-gateway: 引用是否都带 wantstudy/ 前缀"
  # 排除项说明：
  #   .git / node_modules / dist —— 非源码产物
  #   scripts —— 脚本内部的"本地镜像名"（无前缀是合法的本地 tag，不是文档 bug）
  raw_hits="$(grep -rIn \
      --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=dist --exclude-dir=scripts \
      --exclude='*.exe' --exclude='*.gz' --exclude='*.png' --exclude='*.jpg' --exclude='*.svg' \
      -e 'iot-daq-gateway:' "${REPO_ROOT}" 2>/dev/null || true)"
  bad_hits="$(printf '%s\n' "${raw_hits}" | grep -v 'wantstudy/iot-daq-gateway:' | grep -v '^$' || true)"

  if [[ -z "${bad_hits}" ]]; then
    record "⑤" "镜像前缀 wantstudy/ 完整" PASS "未发现缺前缀的镜像引用"
  else
    record "⑤" "镜像前缀 wantstudy/ 完整" FAIL "以下引用缺少 wantstudy/ 前缀（用户 docker run 会拉到本地残留镜像而失败）：
$(printf '%s\n' "${bad_hits}" | sed 's/^/         /')"
  fi
fi

# --------------------------------------------------------------------------
# ⑥ iot-daq 侧 Cargo.toml workspace version == tauri.conf.json version
# --------------------------------------------------------------------------
if group_enabled version; then
  step 6 "iot-daq 源码仓内部版本号一致"
  if [[ -z "${IOT_DAQ_SRC}" ]]; then
    record "⑥" "源码仓版本号一致" SKIP "未找到 iot-daq 源码仓（--version 覆盖模式），跳过"
  else
    ws_ver="$(sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
              "${IOT_DAQ_SRC}/Cargo.toml" | head -n1 || true)"
    shell_ver="$(sed -n 's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
              "${IOT_DAQ_SRC}/deploy/win/tauri-shell/src-tauri/Cargo.toml" | head -n1 || true)"
    tauri_ver="$(read_version)"
    printf '       workspace Cargo.toml : %s\n' "${ws_ver:-<未解析>}"
    printf '       tauri-shell Cargo.toml: %s\n' "${shell_ver:-<未解析>}"
    printf '       tauri.conf.json       : %s\n' "${tauri_ver}"
    if [[ "${ws_ver}" == "${tauri_ver}" && "${shell_ver}" == "${tauri_ver}" ]]; then
      record "⑥" "源码仓版本号一致" PASS "三处均为 ${tauri_ver}"
    else
      record "⑥" "源码仓版本号一致" FAIL "三处不一致（字符串比较）：workspace=${ws_ver} shell=${shell_ver} tauri=${tauri_ver}
       需同步：Cargo.toml [workspace.package].version / deploy/win/tauri-shell/src-tauri/Cargo.toml / tauri.conf.json"
    fi
  fi

  # ------------------------------------------------------------------------
  # ⑦ 官网版本标签 v<VER> 与 tauri 版本一致
  # ------------------------------------------------------------------------
  step 7 "官网版本标签 v${VER} 与 tauri 版本一致"
  if [[ ! -f "${WEBSITE_INDEX}" ]]; then
    record "⑦" "官网版本标签一致" FAIL "找不到 ${WEBSITE_INDEX}"
  else
    # 官网里出现的 vX.Y.Z 形态产品版本标签（自动排除 MQTT v3.1.1 这类协议版本噪声）
    mapfile -t v_tags < <(extract_website_version_tags "${WEBSITE_INDEX}")
    log "忽略的非产品版本标签：${IGNORE_VERSION_TAGS:-（无）}"
    if [[ ${#v_tags[@]} -eq 0 ]]; then
      record "⑦" "官网版本标签一致" FAIL "官网里没有找到 vX.Y.Z 形态的版本标签"
    else
      bad_tag=0
      for t in "${v_tags[@]}"; do
        [[ "${t}" == "v${VER}" ]] || bad_tag=1
      done
      if [[ ${bad_tag} -eq 0 ]]; then
        record "⑦" "官网版本标签一致" PASS "官网出现 ${#v_tags[@]} 种标签，均为 v${VER}"
      else
        record "⑦" "官网版本标签一致" FAIL "官网标签：$(printf '%s ' "${v_tags[@]}")
       期望值：v${VER}"
      fi
    fi
  fi
fi

# --------------------------------------------------------------------------
# 汇总
# --------------------------------------------------------------------------
N_TOTAL=$((N_PASS + N_FAIL))
printf '\n==========================================================================\n'
printf ' 汇总：%s/%s 通过' "${N_PASS}" "${N_TOTAL}"
if [[ ${#RES_ID[@]} -gt ${N_TOTAL} ]]; then
  printf '（另有 %s 项 SKIP）' "$(( ${#RES_ID[@]} - N_TOTAL ))"
fi
printf '\n==========================================================================\n'
i=0
while [[ ${i} -lt ${#RES_ID[@]} ]]; do
  printf '  %-4s %-34s %s\n' "${RES_ID[$i]}" "${RES_NAME[$i]}" "${RES_STATUS[$i]}"
  i=$((i + 1))
done
printf '==========================================================================\n'

if [[ ${N_FAIL} -gt 0 ]]; then
  printf '\n❌ 一致性门禁未通过：%s 项 FAIL。请修正后再发布。\n' "${N_FAIL}"
  exit 1
fi
printf '\n✅ 一致性门禁全部通过（%s/%s）。\n' "${N_PASS}" "${N_TOTAL}"
exit 0
