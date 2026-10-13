#!/usr/bin/env bash
# =============================================================================
# publish.sh —— 发布到外部（Docker Hub / 官网服务器 / 分发仓 git / 服务器离线镜像）
#
# 默认 **--dry-run**：只打印将要执行的命令，不产生任何副作用。
# 真正执行必须显式加 --execute。
#
# 子任务（默认全部关闭需要网络的危险项，按需开启）：
#   [docker]  docker push 双 tag（可用 --skip-docker 关闭，默认开启 push）
#   [website] 需要 --website：官网 vite build + scp 部署 + nginx reload
#   [git]     需要 --git：分发仓 git commit + push（message 带 iot-daq commit hash）
#   [server]  需要 --server-load：docker save | gzip | ssh docker load（服务器不可达 Hub）
#
# 用法：
#   bash scripts/publish.sh                          # 全量 dry-run 演练
#   bash scripts/publish.sh --execute                # 真正执行已开启的子任务
#   bash scripts/publish.sh --execute --skip-docker  # 只发布非 docker 部分
#   bash scripts/publish.sh --execute --website --git
#   bash scripts/publish.sh --execute --server-load
#
# 代理说明（关键）：
#   - docker push / git push / scp / ssh **必须保留代理**：本机靠 HTTP_PROXY 出网；
#   - 官网 vite build 是 npm 系操作，**需要清代理**（与 build-windows.sh 同理）；
#   本脚本在这两类步骤之间显式切换，勿删。
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

# --------------------------------------------------------------------------
# 参数
# --------------------------------------------------------------------------
DRY_RUN=1                 # 默认演练
DO_DOCKER=1               # docker push（--skip-docker 关闭）
DO_WEBSITE=0              # 官网部署（--website 开启）
DO_GIT=0                  # 分发仓 commit+push（--git 开启）
DO_SERVER_LOAD=0          # 服务器离线导入镜像（--server-load 开启）
VERSION_OVERRIDE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)      DRY_RUN=1; shift ;;
    --execute)      DRY_RUN=0; shift ;;
    --skip-docker)  DO_DOCKER=0; shift ;;
    --website)      DO_WEBSITE=1; shift ;;
    --git)          DO_GIT=1; shift ;;
    --server-load)  DO_SERVER_LOAD=1; shift ;;
    --version)      VERSION_OVERRIDE="${2:-}"; shift 2 ;;
    --version=*)    VERSION_OVERRIDE="${1#*=}"; shift ;;
    -h|--help)      sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

LOCAL_IMAGE="${LOCAL_IMAGE_PREFIX}:${VER}"
IMAGE_VER="${IMAGE_REPO}:${VER}"
IOT_DAQ_HASH="$(git -C "${IOT_DAQ_SRC}" rev-parse --short HEAD 2>/dev/null || printf 'unknown')"

printf '\n==========================================================================\n'
printf ' publish —— 版本 v%s（iot-daq@%s）\n' "${VER}" "${IOT_DAQ_HASH}"
printf '==========================================================================\n'
if [[ "${DRY_RUN}" == "1" ]]; then
  printf ' 模式：DRY-RUN（只打印命令，不执行任何副作用）\n'
  printf '       真正执行请加 --execute\n'
else
  printf ' 模式：EXECUTE（将真实执行下列子任务）\n'
fi
printf ' 子任务：docker-push=%s  website=%s  git=%s  server-load=%s\n' \
  "${DO_DOCKER}" "${DO_WEBSITE}" "${DO_GIT}" "${DO_SERVER_LOAD}"
printf '==========================================================================\n'

# --------------------------------------------------------------------------
# 0) 发布前门禁：先跑一致性检查（非 exe 无关项也要过）
# --------------------------------------------------------------------------
step 0 "发布前一致性门禁"
if [[ "${DRY_RUN}" == "1" ]]; then
  log "dry-run 模式仍会**只读**执行一致性检查（它不产生副作用）"
fi
bash "${SCRIPT_DIR}/check-consistency.sh" --version "${VER}" || {
  printf '\n❌ 一致性门禁未通过，发布中止（先修 README/官网/.env 里的引用）。\n'
  exit 1
}

# --------------------------------------------------------------------------
# 1) Docker Hub push（双 tag）
# --------------------------------------------------------------------------
if [[ "${DO_DOCKER}" == "1" ]]; then
  step 1 "推送镜像到 Docker Hub（双 tag）"
  restore_proxy_env          # docker push 必须保留代理
  need_cmd docker
  if [[ "${DRY_RUN}" == "1" ]]; then
    run_cmd "推送版本 tag" docker push "${IMAGE_VER}"
    run_cmd "推送 latest tag" docker push "${IMAGE_LATEST}"
  else
    docker images --format '{{.Repository}}:{{.Tag}}' | grep -Fxq "${IMAGE_VER}" \
      || die "本地没有镜像 ${IMAGE_VER}，请先跑 scripts/build-docker.sh"
    docker push "${IMAGE_VER}"
    docker push "${IMAGE_LATEST}"
    ok "已推送 ${IMAGE_VER} / ${IMAGE_LATEST}"
  fi
else
  step 1 "跳过 docker push（--skip-docker）"
fi

# --------------------------------------------------------------------------
# 2) 官网构建 + 部署 + nginx reload
# --------------------------------------------------------------------------
if [[ "${DO_WEBSITE}" == "1" ]]; then
  step 2 "官网构建与部署"
  need_cmd node ssh scp
  # 官网构建是 npm/vite 系操作 -> 清代理（与 Rust/npm 步骤一致）
  unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
  [[ -x "${WEBSITE_DIR}/node_modules/vite/bin/vite.js" ]] \
    || die "找不到 vite：${WEBSITE_DIR}/node_modules/vite/bin/vite.js（先在 website/ 下 npm i）"
  run_cmd "官网构建（用 vite 直接跑，跳过 vue-tsc 对死 SPA 的检查）" \
    bash -c "cd '${WEBSITE_DIR}' && node node_modules/vite/bin/vite.js build"

  # 部署是 scp/ssh -> 恢复代理
  restore_proxy_env
  run_cmd "上传 dist 到服务器 ${SERVER_WEB_ROOT}" \
    scp -r -o BatchMode=yes "${WEBSITE_DIR}/dist/." "${SERVER_SSH_HOST}:${SERVER_WEB_ROOT}"
  run_cmd "服务器重载 nginx" \
    ssh -o BatchMode=yes "${SERVER_SSH_HOST}" 'nginx -s reload'
  if [[ "${DRY_RUN}" == "0" ]]; then ok "官网已部署并重载 nginx"; fi
else
  step 2 "跳过官网部署（需要 --website）"
fi

# --------------------------------------------------------------------------
# 3) 分发仓 commit + push
# --------------------------------------------------------------------------
if [[ "${DO_GIT}" == "1" ]]; then
  step 3 "提交并推送分发仓"
  restore_proxy_env          # git push 必须保留代理
  need_cmd git
  [[ -f "${GIT_SSH_KEY}" ]] || warn "SSH key 不存在：${GIT_SSH_KEY}（可用 GIT_SSH_KEY 环境变量覆盖）"
  COMMIT_MSG="release: v${VER} (iot-daq@${IOT_DAQ_HASH})"
  run_cmd "提交分发仓变更" git -C "${REPO_ROOT}" add -A
  run_cmd "写提交信息" git -C "${REPO_ROOT}" commit -m "${COMMIT_MSG}"
  run_cmd "推送到 origin main（走 443 SSH）" \
    env GIT_SSH_COMMAND="ssh -i ${GIT_SSH_KEY} -o StrictHostKeyChecking=accept-new" \
        git -C "${REPO_ROOT}" push origin main
  if [[ "${DRY_RUN}" == "0" ]]; then ok "分发仓已推送：${COMMIT_MSG}"; fi
else
  step 3 "跳过分发仓 git 提交（需要 --git）"
fi

# --------------------------------------------------------------------------
# 4) 服务器离线镜像导入（服务器不可达 Docker Hub）
# --------------------------------------------------------------------------
if [[ "${DO_SERVER_LOAD}" == "1" ]]; then
  step 4 "服务器离线导入镜像"
  need_cmd docker ssh
  restore_proxy_env
  # 服务器须先删掉同名旧镜像，否则镜像 ID 不变会导致 load 后 tag 不生效
  run_cmd "删除服务器同名旧镜像" \
    ssh -o BatchMode=yes "${SERVER_SSH_HOST}" \
      "docker rmi -f ${IMAGE_VER} ${IMAGE_LATEST} 2>/dev/null || true"
  run_cmd "流式导入镜像（docker save | gzip | ssh docker load）" \
    bash -c "docker save ${IMAGE_VER} | gzip -1 | ssh -o BatchMode=yes ${SERVER_SSH_HOST} 'gunzip | docker load'"
  if [[ "${DRY_RUN}" == "0" ]]; then ok "服务器镜像已导入：${IMAGE_VER}"; fi
else
  step 4 "跳过服务器离线导入（需要 --server-load）"
fi

# --------------------------------------------------------------------------
# 汇总
# --------------------------------------------------------------------------
printf '\n==========================================================================\n'
if [[ "${DRY_RUN}" == "1" ]]; then
  printf ' DRY-RUN 结束：以上命令均未执行。\n'
  printf ' 确认无误后追加 --execute 真正发布，例如：\n'
  printf '   bash scripts/publish.sh --execute --website --git\n'
else
  printf ' 发布执行完毕（v%s）。\n' "${VER}"
fi
printf '==========================================================================\n'
