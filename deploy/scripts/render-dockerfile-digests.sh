#!/usr/bin/env bash
# =============================================================================
# iot-daq · 基础镜像 digest 渲染脚本（task 60 · container-supply-chain §3）
# =============================================================================
# 定位：从 `deploy/base-images.lock.yaml` 读取基础镜像的 tag ↔ digest，
#   把 `deploy/docker/Dockerfile` 中的占位 token 渲染为真实
#   `FROM <repo>:<tag>@sha256:<64hex>` 行（**保留 @sha256: 形式，禁可变 tag**）。
#
# 为什么需要它（为什么 Dockerfile 不直接写死 digest）：
#   supply-chain §3.1 规定「人工不得在业务 Dockerfile 内手写 digest」，
#   digest 由 lock 文件统一管理、CI 在依赖升级流水线中刷新。
#   因此 Dockerfile 内的 FROM 使用**占位 token**，本脚本才是构建前的文档化入口。
#
# ★ fail-closed（本脚本的核心安全属性）：
#   渲染模式（默认）下，若 lock 文件中任一镜像的 digest 为空 / 非法，
#   脚本**非零退出**并给出可操作原因 —— 使「未 pin digest 的构建」不可能静默成功。
#   （这正是当前仓库的状态：digest 有意留空，待 CI 联网后填入真实值。）
#
# 用法：
#   # 校验 lock 结构与 digest 格式（空 digest 允许，仅告警；CI 门禁用，恒 0/非0 反映结构错误）
#   deploy/scripts/render-dockerfile-digests.sh --check
#
#   # 渲染完整 Dockerfile 到 stdout（含真实 digest；空 digest → 非零退出）
#   deploy/scripts/render-dockerfile-digests.sh
#
#   # 渲染并写文件（CI 构建入口）
#   deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
#
# 依赖：bash + awk + sed + grep（均为 POSIX / 常见工具，无第三方依赖，无网络）。
# =============================================================================

set -euo pipefail

readonly SCRIPT_TAG="[render-digests]"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

LOCK_FILE="${REPO_ROOT}/deploy/base-images.lock.yaml"
TEMPLATE_FILE="${REPO_ROOT}/deploy/docker/Dockerfile"
OUTPUT_FILE=""              # 空 = stdout
CHECK_ONLY=0

log()  { echo "${SCRIPT_TAG} $*" >&2; }
fail() { echo "${SCRIPT_TAG} 致命错误：$*" >&2; exit 1; }

usage() {
    cat >&2 <<'EOF'
用法：render-dockerfile-digests.sh [选项]

选项：
  --lock <file>      基础镜像锁文件（默认 deploy/base-images.lock.yaml）
  --template <file>  Dockerfile 模板（默认 deploy/docker/Dockerfile）
  --output <file>    渲染结果写入文件（默认写 stdout）
  --check            仅校验 lock 结构与 digest 格式，不渲染；空 digest 仅告警
  -h, --help         显示本帮助

退出码：
  0  成功（--check 模式下表示结构合法；空 digest 允许）
  1  失败（lock 结构错误 / digest 非法；渲染模式下 digest 为空 = fail-closed）
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --lock)     LOCK_FILE="${2:?--lock 需要一个文件路径}"; shift 2 ;;
        --template) TEMPLATE_FILE="${2:?--template 需要一个文件路径}"; shift 2 ;;
        --output)   OUTPUT_FILE="${2:?--output 需要一个文件路径}"; shift 2 ;;
        --check)    CHECK_ONLY=1; shift ;;
        -h|--help)  usage; exit 0 ;;
        *)          fail "未知参数：$1（见 --help）" ;;
    esac
done

[[ -f "${LOCK_FILE}" ]]     || fail "锁文件不存在：${LOCK_FILE}"
[[ -f "${TEMPLATE_FILE}" ]] || fail "Dockerfile 模板不存在：${TEMPLATE_FILE}"

# -----------------------------------------------------------------------------
# 解析 lock 文件：输出 `stage<TAB>repository<TAB>tag<TAB>digest` 每行一条。
# 轻量 YAML 子集解析（区块以 `- name:` 起始，键值行缩进 + 可选引号）。
# -----------------------------------------------------------------------------
parse_lock() {
    awk '
        function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
        function unq(s) {
            s = trim(s)
            if (s ~ /^".*"$/ || s ~ /^'"'"'.*'"'"'$/) { s = substr(s, 2, length(s) - 2) }
            return s
        }
        function flush() {
            if (have_block) { printf "%s\t%s\t%s\t%s\n", stage, repo, tag, digest }
            stage = ""; repo = ""; tag = ""; digest = ""; have_block = 0
        }
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*-[[:space:]]*name:/ {
            flush()
            have_block = 1
            next
        }
        have_block == 1 {
            line = $0
            if      (line ~ /^[[:space:]]*repository:/) { sub(/^[[:space:]]*repository:[[:space:]]*/, "", line); repo   = unq(line) }
            else if (line ~ /^[[:space:]]*tag:/)        { sub(/^[[:space:]]*tag:[[:space:]]*/,        "", line); tag    = unq(line) }
            else if (line ~ /^[[:space:]]*digest:/)     { sub(/^[[:space:]]*digest:[[:space:]]*/,     "", line); digest = unq(line) }
            else if (line ~ /^[[:space:]]*stage:/)      { sub(/^[[:space:]]*stage:[[:space:]]*/,      "", line); stage  = unq(line) }
        }
        END { flush() }
    ' "${LOCK_FILE}"
}

# 取指定 stage 的条目（repository/tag/digest）；从 stdin 读取 LOCK_DUMP。
entry_for_stage() {
    local want="$1"
    awk -F'\t' -v s="${want}" '$1 == s { print; found = 1 } END { if (!found) exit 1 }'
}

# 校验 digest 字段：空 → 视为未锁定（返回 3）；非空但格式非法 → 返回 4；合法 → 0。
# 合法格式：`sha256:` + 恰好 64 位小写十六进制。
digest_state() {
    local d="$1"
    if [[ -z "${d}" ]]; then
        return 3
    fi
    if [[ ! "${d}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
        return 4
    fi
    return 0
}

LOCK_DUMP="$(parse_lock)"
[[ -n "${LOCK_DUMP}" ]] || fail "锁文件未解析到任何镜像条目（检查 YAML 结构）"

CHECK_FAILED=0
UNPINNED_COUNT=0

# 逐条校验结构与 digest 格式。
while IFS=$'\t' read -r stage repo tag digest; do
    [[ -n "${stage}" ]] || { log "条目缺少 stage 字段（repository=${repo:-?}）"; CHECK_FAILED=1; continue; }
    [[ -n "${repo}"  ]] || { log "stage=${stage} 条目缺少 repository"; CHECK_FAILED=1; continue; }
    [[ -n "${tag}"   ]] || { log "stage=${stage} 条目缺少 tag";        CHECK_FAILED=1; continue; }

    if digest_state "${digest}"; then
        log "stage=${stage} 已锁定：${repo}:${tag}@${digest}"
    else
        case $? in
            3) log "stage=${stage} 未锁定（digest 为空）：${repo}:${tag} —— 需在 CI 填入真实 digest"; UNPINNED_COUNT=$((UNPINNED_COUNT + 1)) ;;
            4) log "stage=${stage} digest 非法（期望 sha256:<64位小写hex>）：'${digest}'"; CHECK_FAILED=1 ;;
        esac
    fi
done <<< "${LOCK_DUMP}"

if [[ "${CHECK_FAILED}" -ne 0 ]]; then
    fail "lock 文件结构 / digest 格式校验失败（见上方逐条原因）"
fi

# --check 模式：结构合法即通过（空 digest 仅告警）。用于 CI 门禁，不会因「尚未 pin」而红。
if [[ "${CHECK_ONLY}" -eq 1 ]]; then
    if [[ "${UNPINNED_COUNT}" -gt 0 ]]; then
        log "校验通过（结构合法）；但有 ${UNPINNED_COUNT} 个基础镜像尚未 pin digest —— 构建前必须填入。"
    else
        log "校验通过：全部基础镜像已 pin digest。"
    fi
    exit 0
fi

# -----------------------------------------------------------------------------
# 渲染模式：fail-closed —— 任一 digest 未锁定即拒绝。
# -----------------------------------------------------------------------------
if [[ "${UNPINNED_COUNT}" -gt 0 ]]; then
    fail "存在 ${UNPINNED_COUNT} 个未 pin digest 的基础镜像 → 拒绝渲染（fail-closed）。
   现象：deploy/base-images.lock.yaml 中部分 image 的 digest 字段为空。
   原因：digest 尚未由 CI 依赖升级流水线填入（当前仓库的预期状态）。
   后果：若放行，将构建出「可变 tag」镜像，破坏供应链信任链首环（supply-chain §3）。
   修复：在具备网络的环境解析各基础镜像真实 digest 后填入 lock 文件，
     例如： docker buildx imagetools inspect <repo>:<tag> --format '{{.Manifest.Digest}}'
     再重跑本脚本。"
fi

# 提取 builder / runtime 的 repo:tag@sha256:hex。
BUILDER_ENTRY="$(entry_for_stage builder <<< "${LOCK_DUMP}")" || fail "lock 文件缺少 stage=builder 的条目"
RUNTIME_ENTRY="$(entry_for_stage runtime <<< "${LOCK_DUMP}")" || fail "lock 文件缺少 stage=runtime 的条目"

IFS=$'\t' read -r _ b_repo b_tag b_digest <<< "${BUILDER_ENTRY}"
IFS=$'\t' read -r _ r_repo r_tag r_digest <<< "${RUNTIME_ENTRY}"

# 模板中保留 `@sha256:` 字面量，token 仅代表 64 位 hex。
B_HEX="${b_digest#sha256:}"
R_HEX="${r_digest#sha256:}"

RENDERED="$(sed \
    -e "s|__BUILDER_DIGEST__|${B_HEX}|g" \
    -e "s|__RUNTIME_DIGEST__|${R_HEX}|g" \
    "${TEMPLATE_FILE}")"

# 渲染后自检：不得残留占位 token。
if grep -q '__BUILDER_DIGEST__\|__RUNTIME_DIGEST__' <<< "${RENDERED}"; then
    fail "渲染后仍残留占位 token —— 模板 token 名称与脚本不一致（检查 Dockerfile）"
fi

if [[ -n "${OUTPUT_FILE}" ]]; then
    mkdir -p "$(dirname "${OUTPUT_FILE}")"
    printf '%s\n' "${RENDERED}" > "${OUTPUT_FILE}"
    log "已渲染 → ${OUTPUT_FILE}"
    log "  builder: ${b_repo}:${b_tag}@${b_digest}"
    log "  runtime: ${r_repo}:${r_tag}@${r_digest}"
else
    printf '%s\n' "${RENDERED}"
fi
