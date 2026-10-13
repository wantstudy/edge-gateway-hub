#!/usr/bin/env bash
# =============================================================================
# build-docker.sh —— 构建 Docker 镜像并打 Docker Hub 双 tag（只构建，不 push）
#
# 链路：
#   1) 前端：frontends/web-console 用 vite build，产物同步到
#      deploy/docker/context/web-console-dist/，再跑守卫脚本（占位页会 fail-closed）
#      —— 这步必须做，否则镜像里是占位页
#   2) 渲染 Dockerfile digest 占位符 -> Dockerfile.rendered
#   3) docker buildx build --platform linux/amd64 ... --load（本机 Windows 必须 --load）
#      构建上下文 = 仓库根 ${IOT_DAQ_SRC}（不是 crates/）
#   4) 打 Hub 双 tag：wantstudy/iot-daq-gateway:<VER> 与 :latest
#
# 用法：
#   bash scripts/build-docker.sh
#   bash scripts/build-docker.sh --no-frontend      # 前端未变时跳过重建（加速）
#   bash scripts/build-docker.sh --platform linux/amd64
#   bash scripts/build-docker.sh --version 0.2.1
#
# 注意：docker 步骤**不清代理**（本机靠 HTTP_PROXY 出网拉基础镜像）。
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

# --------------------------------------------------------------------------
# 参数
# --------------------------------------------------------------------------
BUILD_FRONTEND=1
PLATFORM="linux/amd64"
VERSION_OVERRIDE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-frontend)  BUILD_FRONTEND=0; shift ;;
    --platform)     PLATFORM="${2:-}"; shift 2 ;;
    --platform=*)   PLATFORM="${1#*=}"; shift ;;
    --version)      VERSION_OVERRIDE="${2:-}"; shift 2 ;;
    --version=*)    VERSION_OVERRIDE="${1#*=}"; shift ;;
    -h|--help)      sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)              die "未知参数：$1（用 --help 查看用法）" ;;
  esac
done

require_iot_daq_src
need_cmd docker

if [[ -n "${VERSION_OVERRIDE}" ]]; then
  VER="${VERSION_OVERRIDE}"
  valid_version_string "${VER}" || die "--version 格式非法：${VER}"
else
  VER="$(read_version)"
fi

LOCAL_IMAGE="${LOCAL_IMAGE_PREFIX}:${VER}"
IMAGE_VER="${IMAGE_REPO}:${VER}"
RENDERED="${IOT_DAQ_SRC}/deploy/docker/Dockerfile.rendered"
RENDER_SCRIPT="${IOT_DAQ_SRC}/deploy/scripts/render-dockerfile-digests.sh"
WEB_DIST="${IOT_DAQ_SRC}/deploy/docker/context/web-console-dist"
GUARD_SCRIPT="${IOT_DAQ_SRC}/deploy/scripts/check-web-console-dist.sh"
BUILD_CTX_WIN="$(to_win_path "${IOT_DAQ_SRC}")"

printf '\n==========================================================================\n'
printf ' build-docker —— 构建镜像（平台 %s，版本 v%s）\n' "${PLATFORM}" "${VER}"
printf '==========================================================================\n'
log "构建上下文 : ${IOT_DAQ_SRC}"
log "Dockerfile : ${RENDERED}"
log "本地镜像   : ${LOCAL_IMAGE}"
log "Hub 双 tag : ${IMAGE_VER} / ${IMAGE_LATEST}"

# --------------------------------------------------------------------------
# 0) docker 出网环境：恢复代理（与 Rust 构建步骤相反，这里必须保留代理）
# --------------------------------------------------------------------------
step 0 "恢复出网代理（docker 拉基础镜像需要；切勿 unset）"
restore_proxy_env
# 坑：MSYS2_ARG_CONV_EXCL="*" 会连 -f 参数一起禁掉路径转换，只留给 docker run 的挂载参数
unset MSYS2_ARG_CONV_EXCL
docker version --format '{{.Server.Version}}' >/dev/null 2>&1 \
  || die "docker 守护进程不可达，请确认 Docker Desktop 已启动"
ok "docker 守护进程可达"

# --------------------------------------------------------------------------
# 1) 前端构建 + 同步 + 守卫（否则镜像里是占位页）
# --------------------------------------------------------------------------
if [[ "${BUILD_FRONTEND}" == "1" ]]; then
  step 1 "构建前端 web-console 并同步到镜像上下文"
  need_cmd npx
  [[ -d "${IOT_DAQ_SRC}/frontends/web-console" ]] \
    || die "找不到前端目录：${IOT_DAQ_SRC}/frontends/web-console"
  # 前端构建走 npm 源，本机代理会干扰 -> 这一步清代理
  unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
  ( cd "${IOT_DAQ_SRC}/frontends/web-console" && npx vite build )
  mkdir -p "${WEB_DIST}"
  cp -r "${IOT_DAQ_SRC}/frontends/web-console/dist/." "${WEB_DIST}/"
  ok "前端产物已同步：${WEB_DIST}"
  # 恢复代理，守卫脚本与后续 docker 步骤可能需要网络
  restore_proxy_env

  step 1b "守卫检查：确认不是占位页"
  [[ -f "${GUARD_SCRIPT}" ]] || die "找不到守卫脚本：${GUARD_SCRIPT}"
  bash "${GUARD_SCRIPT}" || die "前端产物守卫检查未通过（疑似占位页），禁止继续构建镜像"
  ok "前端产物守卫通过"
else
  step 1 "跳过前端重建（--no-frontend），沿用现有 ${WEB_DIST}"
  [[ -d "${WEB_DIST}" ]] || die "前端产物目录不存在：${WEB_DIST}（不能用 --no-frontend）"
  if [[ -f "${GUARD_SCRIPT}" ]]; then
    bash "${GUARD_SCRIPT}" || die "现有前端产物是占位页，禁止构建镜像"
    ok "前端产物守卫通过"
  fi
fi

# --------------------------------------------------------------------------
# 2) 渲染 Dockerfile digest
# --------------------------------------------------------------------------
step 2 "渲染 Dockerfile digest 占位符"
[[ -f "${RENDER_SCRIPT}" ]] || die "找不到渲染脚本：${RENDER_SCRIPT}"
bash "${RENDER_SCRIPT}" --output "${RENDERED}"
[[ -f "${RENDERED}" ]] || die "渲染产物缺失：${RENDERED}"
if grep -q '__BUILDER_DIGEST__' "${RENDERED}"; then
  die "Dockerfile.rendered 仍残留 __BUILDER_DIGEST__ 占位符，拒绝构建"
fi
ok "已生成：${RENDERED}"

# --------------------------------------------------------------------------
# 3) buildx build --load
# --------------------------------------------------------------------------
step 3 "docker buildx build（耗时 6~25 分钟）"
DOCKERFILE_WIN="$(to_win_path "${RENDERED}")"
printf '  $ docker buildx build --platform %s --file %s \\\n' "${PLATFORM}" "${DOCKERFILE_WIN}"
printf '      -t %s --load %s\n' "${LOCAL_IMAGE}" "${BUILD_CTX_WIN}"
docker buildx build \
  --platform "${PLATFORM}" \
  --file "${DOCKERFILE_WIN}" \
  -t "${LOCAL_IMAGE}" \
  --load \
  "${BUILD_CTX_WIN}"
ok "镜像已构建并载入本地：${LOCAL_IMAGE}"

# --------------------------------------------------------------------------
# 4) 打 Hub 双 tag
# --------------------------------------------------------------------------
step 4 "打 Docker Hub 双 tag"
docker tag "${LOCAL_IMAGE}" "${IMAGE_VER}"
docker tag "${LOCAL_IMAGE}" "${IMAGE_LATEST}"
ok "${IMAGE_VER}"
ok "${IMAGE_LATEST}"

printf '\n 本机镜像清单：\n'
docker images --format '    {{.Repository}}:{{.Tag}}  {{.ID}}  {{.Size}}' \
  | grep 'iot-daq-gateway' || true

printf '\n✅ build-docker 完成：v%s\n' "${VER}"
printf '   下一步：bash scripts/publish.sh --dry-run（先演练），确认无误后加 --execute\n'
