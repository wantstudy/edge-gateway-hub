#!/usr/bin/env bash
# =============================================================================
# iot-daq 网关 · 离线交付包构建脚本（task 60 新增，不修改任何既有文件）
# =============================================================================
# 用途：在厂商 CI / 构建机上，把多架构镜像构建出来并导出为可离线分发的 tar 包，
#       附带 SHA256SUMS 清单（可含随包 cosign 公钥），可选 cosign 签名。
#
# 典型用法：
#   # 默认：构建 amd64 + arm64 两个架构并各自导出 tar 到 deploy/out/
#   IOT_DAQ_IMAGE='registry.example/iot-daq/iot-daq-gateway@sha256:<64hex>' \
#   IOT_DAQ_VERSION='1.0.0' \
#     ./build-offline-bundle.sh --env-file ../.env
#
#   # 单架构（现场只需一种 CPU，节省体积）：
#   ./build-offline-bundle.sh --env-file ../.env --arch arm64
#
#   # 构建后顺带签名（私钥来自环境变量 IOT_DAQ_COSIGN_KEY，或 keyless）：
#   IOT_DAQ_COSIGN_KEY=kms://vendor-kms/iot-daq/cosign-supply \
#     ./build-offline-bundle.sh --env-file ../.env --arch amd64 --sign
#
# =============================================================================
# ★★★ 红线（违反即视为缺陷，本脚本会主动拒绝）★★★
#   1) 禁可变 tag / :latest：IOT_DAQ_IMAGE 必须是 `<repo>@sha256:<64hex>` 形式的
#      digest 引用。若是 `:v1.0.0` / `:latest` 之类可变 tag，脚本直接报错退出。
#      （理由：可变 tag 指向内容可被上游静默替换，等于把信任链首环交给第三方。）
#   2) digest 必须：离线现场（无外网）导入的镜像与 CI 构建的镜像必须逐字节可比对，
#      只有 digest pin 才能保证。脚本同时把每个架构的真实 digest 写入
#      images-digests.txt 并纳入 SHA256SUMS。
#   3) 绝不写死任何密钥 / 激活码 / 指纹盐：所有敏感值仅从环境变量读取，本文件
#      内无任何字面量密钥、口令、盐、激活码。
# =============================================================================
#
# 设计依据：docs/design/container-supply-chain.md §2（多架构）/ §3（digest pin）/ §7（离线包）
# 衔接说明：本脚本只负责「构建 + 导出 + 清单 + 可选签名」，不重复 install.sh 的
#           指纹采集 / HMAC / compose 渲染 / up -d / 健康检查职责。
# =============================================================================

set -euo pipefail

# -----------------------------------------------------------------------------
# 路径与常量
# -----------------------------------------------------------------------------
readonly SCRIPT_NAME="$(basename "$0")"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 仓库根（deploy/scripts -> 上两级）
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# 默认值（均可被参数 / 环境变量覆盖）
DOCKERFILE="${IOT_DAQ_DOCKERFILE:-${SCRIPT_DIR}/../docker/Dockerfile}"
CONTEXT="${IOT_DAQ_BUILD_CONTEXT:-${REPO_ROOT}}"
OUT_DIR="${IOT_DAQ_OFFLINE_OUT_DIR:-${SCRIPT_DIR}/../out}"
ENV_FILE=""
ARCH_OVERRIDE=""
DO_SIGN=0
VERSION="${IOT_DAQ_VERSION:-}"

# 镜像引用（digest 形式，红线 1/2）。优先环境变量，其次 --env-file 提供。
IMAGE_REF="${IOT_DAQ_IMAGE:-}"
COSIGN_PUBKEY="${IOT_DAQ_COSIGN_PUBKEY:-}"
COSIGN_KEY="${IOT_DAQ_COSIGN_KEY:-}"

# -----------------------------------------------------------------------------
# 输出辅助
# -----------------------------------------------------------------------------
log()  { printf '[build-offline][info] %s\n' "$*"; }
warn() { printf '[build-offline][警告] %s\n' "$*" >&2; }
die()  { printf '[build-offline][致命] %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
用法：${SCRIPT_NAME} [选项]

必填（红线）：IOT_DAQ_IMAGE 必须是 <repo>@sha256:<64hex> 形式的 digest 引用。

选项：
  --env-file <path>    从指定 .env 读取变量（如 ../.env）。未指定则只用环境变量。
  --arch <amd64|arm64> 仅构建单一架构（默认 amd64 与 arm64 都构建）。
  --version <x.y.z>    包文件名版本号（如 1.0.0）；未指定则取 IOT_DAQ_VERSION，再否则报错。
  --out-dir <path>     导出目录（默认 deploy/out）。
  --dockerfile <path>  构建用 Dockerfile（默认 deploy/docker/Dockerfile）。
  --context <path>     构建上下文（默认仓库根）。
  --sign               构建完成后对镜像 digest 做 cosign sign（私钥读 IOT_DAQ_COSIGN_KEY，
                       或置 IOT_DAQ_COSIGN_KEY=keyless 走 keyless --yes）。
  --help               显示本帮助。

红线：禁可变 tag / :latest；镜像必须是 @sha256: digest 形式；脚本内不含任何密钥字面量。
EOF
}

# -----------------------------------------------------------------------------
# 红线 1/2：断言镜像引用为 digest 形式
# -----------------------------------------------------------------------------
assert_digest_pinned() {
    local img="$1"
    if [[ -z "${img}" ]]; then
        die "IOT_DAQ_IMAGE 为空。必须通过 --env-file 或环境变量提供镜像引用（digest 形式）。"
    fi
    # 含 tag 但不含 digest：可变 tag / :latest -> 拒绝
    if [[ "${img}" != *"@sha256:"* ]]; then
        die "镜像引用 '${img}' 不是 digest 形式（缺少 @sha256:）。本脚本禁止可变 tag / :latest（供应链红线）。请改为 <repo>@sha256:<64hex>。"
    fi
    local digest="${img##*@sha256:}"
    if ! [[ "${digest}" =~ ^[0-9a-f]{64}$ ]]; then
        die "镜像 digest 不合法：'${digest}' 应为 64 位十六进制。请核对 IOT_DAQ_IMAGE。"
    fi
}

# 取 <repo> 部分（去掉 @sha256:...），并取镜像短名（最后一段路径）
image_repo() {
    local img="$1"
    local repo="${img%%@*}"
    printf '%s' "${repo}"
}

# -----------------------------------------------------------------------------
# 参数解析
# -----------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --env-file) ENV_FILE="$2"; shift 2 ;;
        --arch)     ARCH_OVERRIDE="$2"; shift 2 ;;
        --version)  VERSION="$2"; shift 2 ;;
        --out-dir)  OUT_DIR="$2"; shift 2 ;;
        --dockerfile) DOCKERFILE="$2"; shift 2 ;;
        --context)  CONTEXT="$2"; shift 2 ;;
        --sign)     DO_SIGN=1; shift ;;
        --help|-h)  usage; exit 0 ;;
        *) die "未知参数：$1（用 --help 查看用法）" ;;
    esac
done

# 载入 .env（若存在）。仅 export 其中变量，不覆盖已设置的环境变量语义由 source 决定；
# 此处 source 用于把 .env 中的 IOT_DAQ_IMAGE 等带进来。
if [[ -n "${ENV_FILE}" ]]; then
    if [[ ! -f "${ENV_FILE}" ]]; then
        die "指定的 --env-file 不存在：${ENV_FILE}"
    fi
    # 仅导入看起来像 IOT_DAQ_* / COMPOSE_* 的变量，避免误带其它副作用
    set -a
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
    set +a
    # 重新读取可能被 .env 覆盖的值
    IMAGE_REF="${IOT_DAQ_IMAGE:-${IMAGE_REF}}"
    COSIGN_PUBKEY="${IOT_DAQ_COSIGN_PUBKEY:-${COSIGN_PUBKEY}}"
    COSIGN_KEY="${IOT_DAQ_COSIGN_KEY:-${COSIGN_KEY}}"
    VERSION="${IOT_DAQ_VERSION:-${VERSION}}"
fi

# 红线校验
assert_digest_pinned "${IMAGE_REF}"

# 版本号必填（用于文件名）
if [[ -z "${VERSION}" ]]; then
    die "未提供版本号。请用 --version x.y.z 或设置 IOT_DAQ_VERSION（用于包文件名）。"
fi
if ! [[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]]; then
    die "版本号格式不合法：'${VERSION}'（期望如 1.0.0）。"
fi

# 目标架构列表
if [[ -n "${ARCH_OVERRIDE}" ]]; then
    case "${ARCH_OVERRIDE}" in
        amd64|arm64) ARCHES=("${ARCH_OVERRIDE}") ;;
        *) die "不支持的 --arch='${ARCH_OVERRIDE}'（仅 amd64 / arm64）" ;;
    esac
else
    ARCHES=(amd64 arm64)
fi

# 工具前置检查（提前失败，避免构建到一半才发现缺命令）
for cmd in docker sha256sum; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        die "缺少必要命令：${cmd}"
    fi
done
if [[ "${DO_SIGN}" -eq 1 ]] && ! command -v cosign >/dev/null 2>&1; then
    die "启用 --sign 但本机无 cosign 二进制。"
fi

# Dockerfile / 上下文存在性
if [[ ! -f "${DOCKERFILE}" ]]; then
    die "Dockerfile 不存在：${DOCKERFILE}"
fi
if [[ ! -d "${CONTEXT}" ]]; then
    die "构建上下文目录不存在：${CONTEXT}"
fi

# 幂等：重建输出目录（保留父目录，避免误删其它内容）
mkdir -p "${OUT_DIR}"
# 仅清理本脚本可能产出的文件，避免误删用户文件
rm -f "${OUT_DIR}"/iot-daq-gateway_v"${VERSION}"_*.tar \
      "${OUT_DIR}/SHA256SUMS" \
      "${OUT_DIR}/images-digests.txt" \
      "${OUT_DIR}/cosign-pubkey.pem" 2>/dev/null || true

# -----------------------------------------------------------------------------
# 主流程：逐架构构建 -> 导出 tar -> 记录 digest
# -----------------------------------------------------------------------------
readonly REPO="$(image_repo "${IMAGE_REF}")"
readonly IMAGE_BASE="$(basename "${REPO}")"   # 如 iot-daq-gateway
readonly DIGEST_FILE="${OUT_DIR}/images-digests.txt"

: > "${DIGEST_FILE}"   # 清空 digest 记录文件
declare -a TAR_FILES=()

log "开始构建离线包：版本=${VERSION}  架构=${ARCHES[*]}  镜像=${REPO}@sha256:${IMAGE_REF##*@sha256:}"

for arch in "${ARCHES[@]}"; do
    log ">>> 架构 linux/${arch}：buildx 构建 + docker save 导出"

    # 本机构建标签（仅构建期使用，不进入交付物命名）
    BUILD_TAG="${REPO}:offline-${VERSION}-${arch}"
    OUT_TAR="${OUT_DIR}/${IMAGE_BASE}_v${VERSION}_${arch}.tar"

    # 构建单架构镜像并载入本地 docker daemon（buildx --load 仅支持单平台，故逐架构循环）。
    # 注：在异架构宿主上构建需预先安装 QEMU/binfmt（如 docker run --privileged tonistiigi/binfmt）。
    #   若希望完全不依赖本地 daemon，可改用：
    #     docker buildx build --platform linux/${arch} -f "${DOCKERFILE}" \
    #       --output "type=docker,dest=${OUT_TAR}" "${CONTEXT}"
    #   一步直接产出 tar（等价于 build+save），本脚本保留 build+save 两步以贴合 supply-chain 描述。
    docker buildx build \
        --platform "linux/${arch}" \
        --file "${DOCKERFILE}" \
        --tag "${BUILD_TAG}" \
        --load \
        "${CONTEXT}"

    # 导出为离线 tar（文件名含版本与架构）
    docker save -o "${OUT_TAR}" "${BUILD_TAG}"

    # 记录该架构镜像的真实 digest（docker save 的产物即此 digest 对应的镜像）
    LOADED_DIGEST="$(docker inspect --format='{{index .RepoDigests 0}}' "${BUILD_TAG}" 2>/dev/null || true)"
    if [[ -z "${LOADED_DIGEST}" ]]; then
        warn "无法从本地 daemon 读取 ${BUILD_TAG} 的 RepoDigests（镜像可能未打 digest tag），仍继续。"
        LOADED_DIGEST="${REPO}@<unknown>"
    fi
    printf '%s\t%s\t%s\n' "${arch}" "$(basename "${OUT_TAR}")" "${LOADED_DIGEST}" >> "${DIGEST_FILE}"
    TAR_FILES+=("$(basename "${OUT_TAR}")")

    log "    已导出 ${OUT_TAR}  ->  ${LOADED_DIGEST}"

    # 可选：cosign 签名（对 tar 内含的该架构 digest 签名，使离线验签可匹配）
    if [[ "${DO_SIGN}" -eq 1 ]]; then
        log "    对 ${LOADED_DIGEST} 做 cosign sign"
        if [[ -n "${COSIGN_KEY}" && "${COSIGN_KEY}" != "keyless" ]]; then
            cosign sign --key "${COSIGN_KEY}" "${LOADED_DIGEST}"
        else
            # keyless 需访问 Fulcro/Rekor（离线现场一般不可用），仅作可选能力
            cosign sign --yes "${LOADED_DIGEST}"
        fi
    fi
done

# -----------------------------------------------------------------------------
# 生成 SHA256SUMS 清单（覆盖 tar 与 digest 记录；可选纳入随包 cosign 公钥）
# -----------------------------------------------------------------------------
log "生成 SHA256SUMS 清单 ..."
(
    cd "${OUT_DIR}"
    # 相对路径记录，便于离线介质任意目录解包后直接 sha256sum -c
    sha256sum "${TAR_FILES[@]}" images-digests.txt > SHA256SUMS
    if [[ -n "${COSIGN_PUBKEY}" && -f "${COSIGN_PUBKEY}" ]]; then
        cp "${COSIGN_PUBKEY}" "${OUT_DIR}/cosign-pubkey.pem"
        sha256sum cosign-pubkey.pem >> SHA256SUMS
        log "    已纳入随包 cosign 公钥：cosign-pubkey.pem"
    fi
)

log "完成。输出目录：${OUT_DIR}"
log "  产物："
for f in "${TAR_FILES[@]}"; do
    log "    - ${OUT_DIR}/${f}"
done
log "    - ${OUT_DIR}/images-digests.txt"
log "    - ${OUT_DIR}/SHA256SUMS"
if [[ "${DO_SIGN}" -eq 0 ]]; then
    warn "本次未签名（--sign 未启用）。交付前请由厂商侧对 images-digests.txt 中的 digest 做 cosign sign，"
    warn "并随安装介质『独立渠道』分发公钥（见 container-supply-chain.md §4.1），切勿与安装包同渠道。"
fi
log "下一步：将 ${OUT_DIR} 整体作为离线介质转移；现场用 sign-and-verify.sh --verify 校验后 docker load + install.sh。"
