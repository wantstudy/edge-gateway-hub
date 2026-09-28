#!/usr/bin/env bash
# =============================================================================
# iot-daq 网关 · 镜像签名 / 离线校验脚本（task 60 新增，不修改任何既有文件）
# =============================================================================
# 两个互斥模式：
#   --sign    厂商侧：对镜像 digest 做 cosign sign（私钥读 IOT_DAQ_COSIGN_KEY，或 keyless）。
#   --verify  现场侧：离线包 sha256sum -c + docker load + cosign verify（含 -a git-sha 断言）
#                    + 镜像可变 tag / :latest 直接拒绝。
#
# 典型用法：
#   # 厂商签名（key-pair，私钥经 KMS）
#   IOT_DAQ_IMAGE='...@sha256:<64hex>' IOT_DAQ_RELEASE_GIT_SHA=<sha> \
#   IOT_DAQ_COSIGN_KEY=kms://vendor-kms/iot-daq/cosign-supply \
#     ./sign-and-verify.sh --sign
#
#   # 现场校验离线包
#   IOT_DAQ_IMAGE='...@sha256:<64hex>' IOT_DAQ_RELEASE_GIT_SHA=<sha> \
#   IOT_DAQ_COSIGN_PUBKEY=/opt/iot-daq/offline/cosign/iot-daq-supply.pub \
#   IOT_DAQ_OFFLINE_BUNDLE_DIR=/opt/iot-daq/offline \
#     ./sign-and-verify.sh --verify
# =============================================================================
# ★★★ 红线（违反即拒绝）★★★
#   1) 镜像若用可变 tag / :latest，直接拒绝（无论 sign 还是 verify）。
#   2) 绝不写死任何密钥 / 激活码 / 指纹盐：私钥仅从 IOT_DAQ_COSIGN_KEY 读取，
#      公钥仅从 IOT_DAQ_COSIGN_PUBKEY 读取。
#   3) 校验失败即停：默认 IOT_DAQ_ALLOW_VERIFY_DEGRADE=0 时缺 cosign 即失败；
#      仅当显式置 1 才走降级（仅哈希 + load + 醒目警告 + 记残余风险），且仍不推荐。
# =============================================================================
# 设计依据：docs/design/container-supply-chain.md §4（签名/校验链）/ §7（离线 tar 校验）
# 衔接说明：本脚本只负责「签名 / 校验」，不重复 install.sh 的指纹采集 / compose 渲染 / up -d。
# =============================================================================

set -euo pipefail

# -----------------------------------------------------------------------------
# 路径与常量
# -----------------------------------------------------------------------------
readonly SCRIPT_NAME="$(basename "$0")"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 运行时变量（来自环境变量 / --env-file）
IMAGE_REF="${IOT_DAQ_IMAGE:-}"
GIT_SHA="${IOT_DAQ_RELEASE_GIT_SHA:-}"
PUBKEY="${IOT_DAQ_COSIGN_PUBKEY:-}"
SIGN_KEY="${IOT_DAQ_COSIGN_KEY:-}"
BUNDLE_DIR="${IOT_DAQ_OFFLINE_BUNDLE_DIR:-}"
ENV_FILE=""
MODE=""
ALLOW_DEGRADE="${IOT_DAQ_ALLOW_VERIFY_DEGRADE:-0}"

# -----------------------------------------------------------------------------
# 输出辅助
# -----------------------------------------------------------------------------
log()  { printf '[sign-verify][info] %s\n' "$*"; }
warn() { printf '[sign-verify][警告] %s\n' "$*" >&2; }
die()  { printf '[sign-verify][致命] %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
用法：${SCRIPT_NAME} --sign | --verify [选项]

模式（二选一）：
  --sign     对 IOT_DAQ_IMAGE（digest 形式）做 cosign sign。
  --verify   校验离线包：sha256sum -c + docker load + cosign verify。

通用选项：
  --env-file <path>  从指定 .env 读取变量（如 ../.env）。
  --help             显示本帮助。

关键环境变量（也可经 --env-file 注入）：
  IOT_DAQ_IMAGE             镜像 digest 引用（必填，红线：禁可变 tag）。
  IOT_DAQ_RELEASE_GIT_SHA   发布 git-sha（-a 断言，verify 时必填）。
  IOT_DAQ_COSIGN_PUBKEY     验签公钥路径（verify 必填）。
  IOT_DAQ_COSIGN_KEY        签名私钥 / kms://...（sign 用；置 keyless 走 keyless --yes）。
  IOT_DAQ_OFFLINE_BUNDLE_DIR 离线包目录（verify 用，默认空=当前目录）。
  IOT_DAQ_ALLOW_VERIFY_DEGRADE 0/1（默认 0；置 1 允许无 cosign 降级，不推荐）。
EOF
}

# -----------------------------------------------------------------------------
# 红线 1：断言镜像引用为 digest 形式（可变 tag / :latest 拒绝）
# -----------------------------------------------------------------------------
assert_digest_pinned() {
    local img="$1"
    if [[ -z "${img}" ]]; then
        die "IOT_DAQ_IMAGE 为空，必须提供 digest 形式镜像引用。"
    fi
    if [[ "${img}" != *"@sha256:"* ]]; then
        die "镜像引用 '${img}' 不是 digest 形式（缺少 @sha256:）。禁可变 tag / :latest（供应链红线）。"
    fi
    local digest="${img##*@sha256:}"
    if ! [[ "${digest}" =~ ^[0-9a-f]{64}$ ]]; then
        die "镜像 digest 不合法：'${digest}' 应为 64 位十六进制。"
    fi
}

# -----------------------------------------------------------------------------
# 参数解析
# -----------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sign)    MODE="sign"; shift ;;
        --verify)  MODE="verify"; shift ;;
        --env-file) ENV_FILE="$2"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) die "未知参数：$1（用 --help 查看用法）" ;;
    esac
done

if [[ -z "${MODE}" ]]; then
    die "必须指定 --sign 或 --verify 之一。"
fi

if [[ -n "${ENV_FILE}" ]]; then
    if [[ ! -f "${ENV_FILE}" ]]; then
        die "指定的 --env-file 不存在：${ENV_FILE}"
    fi
    set -a
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
    set +a
    IMAGE_REF="${IOT_DAQ_IMAGE:-${IMAGE_REF}}"
    GIT_SHA="${IOT_DAQ_RELEASE_GIT_SHA:-${GIT_SHA}}"
    PUBKEY="${IOT_DAQ_COSIGN_PUBKEY:-${PUBKEY}}"
    SIGN_KEY="${IOT_DAQ_COSIGN_KEY:-${SIGN_KEY}}"
    BUNDLE_DIR="${IOT_DAQ_OFFLINE_BUNDLE_DIR:-${BUNDLE_DIR}}"
    ALLOW_DEGRADE="${IOT_DAQ_ALLOW_VERIFY_DEGRADE:-${ALLOW_DEGRADE}}"
fi

# 红线校验（两模式通用）
assert_digest_pinned "${IMAGE_REF}"

# =============================================================================
# 模式：--sign（厂商侧）
# =============================================================================
do_sign() {
    if ! command -v cosign >/dev/null 2>&1; then
        die "sign 模式需要 cosign 二进制，但本机未找到。"
    fi
    if [[ -z "${SIGN_KEY}" ]]; then
        die "sign 模式需要 IOT_DAQ_COSIGN_KEY（私钥 / kms://...，或置 keyless 走 keyless --yes）。"
    fi

    log "对 ${IMAGE_REF} 做 cosign sign（-a git-sha=${GIT_SHA:-<未提供>})"
    if [[ "${SIGN_KEY}" == "keyless" ]]; then
        # keyless 需访问 Fulcro/Rekor（离线现场不可用）；此处仅作可选能力
        cosign sign --yes "${IMAGE_REF}"
    else
        if [[ -n "${GIT_SHA}" ]]; then
            cosign sign --key "${SIGN_KEY}" -a git-sha="${GIT_SHA}" "${IMAGE_REF}"
        else
            cosign sign --key "${SIGN_KEY}" "${IMAGE_REF}"
        fi
    fi
    log "签名完成。请随安装介质『独立渠道』分发公钥（container-supply-chain.md §4.1）。"
}

# =============================================================================
# 模式：--verify（现场侧）
# =============================================================================
do_verify() {
    # 1) 哈希清单校验（离线安全，最先做，失败即停）
    if [[ -z "${BUNDLE_DIR}" ]]; then
        BUNDLE_DIR="."
    fi
    if [[ ! -d "${BUNDLE_DIR}" ]]; then
        die "离线包目录不存在：${BUNDLE_DIR}（请设置 IOT_DAQ_OFFLINE_BUNDLE_DIR 或 --env-file）。"
    fi
    local sums="${BUNDLE_DIR}/SHA256SUMS"
    if [[ ! -f "${sums}" ]]; then
        die "离线包内未找到 SHA256SUMS：${sums}"
    fi
    log "[1/3] 哈希清单校验（sha256sum -c ${sums}）"
    (
        cd "${BUNDLE_DIR}"
        sha256sum -c SHA256SUMS
    ) || die "SHA256SUMS 校验失败：离线包已被改动或下载不完整，整包作废，从厂商渠道重新获取。"

    # 2) 导入镜像（docker load）
    log "[2/3] 导入镜像（docker load）"
    if ! command -v docker >/dev/null 2>&1; then
        die "verify 需要 docker 二进制，但本机未找到。"
    fi
    shopt -s nullglob
    local tars=()
    for p in "${BUNDLE_DIR}"/*.tar "${BUNDLE_DIR}"/images/*.tar; do
        tars+=("$p")
    done
    shopt -u nullglob
    if [[ ${#tars[@]} -eq 0 ]]; then
        die "离线包内未找到任何 *.tar 镜像文件（应在 ${BUNDLE_DIR} 或 ${BUNDLE_DIR}/images/）。"
    fi
    for tar in "${tars[@]}"; do
        log "    load ${tar}"
        docker load -i "${tar}"
    done

    # 3) cosign 验签（+ git-sha 断言）；缺 cosign 时按降级开关处理
    log "[3/3] cosign 验签（含 -a git-sha 断言）"
    local have_cosign=0
    if command -v cosign >/dev/null 2>&1; then
        have_cosign=1
    fi

    if [[ "${have_cosign}" -eq 0 ]]; then
        # 缺 cosign：默认失败；仅 IOT_DAQ_ALLOW_VERIFY_DEGRADE=1 走降级
        if [[ "${ALLOW_DEGRADE}" != "1" ]]; then
            die "本机无 cosign 二进制，且 IOT_DAQ_ALLOW_VERIFY_DEGRADE=0（默认）。拒绝继续安装：无法确认镜像来源真实性。请先部署 cosign 或用独立渠道取得的验签结果。"
        fi
        print_degrade_warning
        record_residual_risk "${BUNDLE_DIR}" \
            "verify 阶段 cosign 缺失（IOT_DAQ_ALLOW_VERIFY_DEGRADE=1）：仅完成 sha256sum -c 与 docker load，未完成 cosign 验签。"
        log "降级完成：哈希与 load 已通过，但镜像未经验签。请人工按手册核对 digest 指纹后谨慎放行。"
        return 0
    fi

    # 确定公钥：优先 IOT_DAQ_COSIGN_PUBKEY，否则尝试包内 cosign-pubkey.pem
    local key_arg="${PUBKEY}"
    if [[ -z "${key_arg}" || ! -f "${key_arg}" ]]; then
        if [[ -f "${BUNDLE_DIR}/cosign-pubkey.pem" ]]; then
            key_arg="${BUNDLE_DIR}/cosign-pubkey.pem"
            warn "IOT_DAQ_COSIGN_PUBKEY 未指向有效文件，改用包内 cosign-pubkey.pem（请确认该公钥来自独立渠道）。"
        fi
    fi
    if [[ -z "${key_arg}" || ! -f "${key_arg}" ]]; then
        die "未找到验签公钥。请设置 IOT_DAQ_COSIGN_PUBKEY 指向有效公钥文件（且应来自独立分发渠道）。"
    fi

    if [[ -z "${GIT_SHA}" ]]; then
        die "IOT_DAQ_RELEASE_GIT_SHA 为空，无法做 -a git-sha 断言（供应链要求）。"
    fi

    log "    cosign verify --key ${key_arg} -a git-sha=${GIT_SHA} ${IMAGE_REF}"
    if ! cosign verify --key "${key_arg}" -a "git-sha=${GIT_SHA}" "${IMAGE_REF}"; then
        die "cosign 验签失败：镜像来源/完整性不可信，拒绝安装。整包作废，从厂商渠道重新获取。"
    fi
    log "OK: 供应链校验通过（哈希 + 验签 + git-sha 断言一致）。可执行 host-fingerprint-collect.sh 后 docker compose up -d。"
}

# 醒目降级警告（纯文本，无 emoji）
print_degrade_warning() {
    cat >&2 <<'EOF'

!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
!! 警告：未执行 cosign 验签（降级路径，IOT_DAQ_ALLOW_VERIFY_DEGRADE=1）!!
!! 这意味着无法确认镜像是否来自厂商、是否被篡改。                     !!
!! 仅哈希 + load 通过，不提供来源真实性保证。                       !!
!! 残余风险已记录。请由具备权限的人员按手册核对 digest 指纹后放行。   !!
!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!

EOF
}

record_residual_risk() {
    local bundle="$1" reason="$2"
    local stamp
    stamp="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
    local out="${bundle}/RESIDUAL_RISK.verify.log"
    {
        echo "[${stamp}] 残余风险（verify 降级）"
        echo "  镜像引用: ${IMAGE_REF}"
        echo "  原因: ${reason}"
        echo "  处置: 需人工核对 digest 指纹，并记录于现场安装单。"
    } >> "${out}" 2>/dev/null || true
    warn "残余风险已写入：${out}"
}

# -----------------------------------------------------------------------------
# 派发
# -----------------------------------------------------------------------------
case "${MODE}" in
    sign)   do_sign ;;
    verify) do_verify ;;
esac
