#!/usr/bin/env bash
# =============================================================================
# iot-daq · web-console 交付物守卫脚本（fail-closed）
# =============================================================================
# 守的红线：
#   deploy/docker/context/web-console-dist/index.html 必须是**真实的前端产物**。
#   绝不允许把写着 "TEST_ONLY placeholder" 的占位页打进镜像、当作交付物发出去。
#
# 为什么以前会漏（fail-open → fail-closed 的由来）：
#   deploy/docker/Dockerfile:98 执行 `COPY deploy/docker/context/web-console-dist/ ...`。
#   该目录因为占位页被提交而**存在**，所以 COPY 不失败；占位页随后被
#   Dockerfile:163 复制进 /srv/web-console/。结果是 docker build 显示成功、
#   CI 显示成功，而容器里跑的是写着 "placeholder — replaced by CI build" 的假界面。
#   这类事故没有任何报错信号：日志干净、退出码为 0，只有打开页面才会被发现。
#
#   Dockerfile 第 95-98 行的注释原口径是「目录缺失 = 构建失败（fail-closed）」，
#   该口径只对「目录被删掉」成立，对「目录里有假货」并不成立 —— 本脚本补上后半段：
#   文件缺失要拒绝，内容是占位页同样要拒绝，两条路都不允许静默通过。
#
# 接入点（fail-closed 的落地位置）：
#   - CI      .github/workflows/build.yml 的 deploy job（digest 锁文件校验之后）
#   - 离线包  deploy/scripts/build-offline-bundle.sh 开头（任何实质工作之前）
#
# 退出码：
#   0  通过：目标存在且不是占位页
#   1  拒绝：目标缺失，或内容仍是占位页（交付物不可用，调用方须立即非零退出）
# =============================================================================

set -euo pipefail

readonly SCRIPT_NAME="check-web-console-dist"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 仓库根（deploy/scripts -> 上两级）
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# 被守卫的交付物本体。
readonly TARGET="${REPO_ROOT}/deploy/docker/context/web-console-dist/index.html"

log()  { printf '[%s] %s\n' "${SCRIPT_NAME}" "$*" >&2; }
fail() { printf '[%s] 致命错误：%s\n' "${SCRIPT_NAME}" "$*" >&2; exit 1; }

# -----------------------------------------------------------------------------
# 守卫 1：目标文件必须存在。
# -----------------------------------------------------------------------------
if [[ ! -f "${TARGET}" ]]; then
    fail "web-console 前端产物缺失（fail-closed）：${TARGET}
现象：目标文件不存在。
后果：Dockerfile:98 的 COPY 在目录缺失时本就该让构建失败；若放行，
     deploy/docker/context/web-console-dist/ 就成了一个哑目录，镜像里很可能没有前端。
修复：先构建真实前端产物（见下），再把产物提供到本路径。"
fi

# -----------------------------------------------------------------------------
# 守卫 2：内容不得仍是占位页。
# -----------------------------------------------------------------------------
# 特征串取自被提交的占位页原文，命中任一即判定为「交付假前端」。
# 注意：占位页注释里承诺「CI / build-offline-bundle.sh 会以真实产物覆盖本目录」，
# 但仓库里既没有覆盖它的 CI 步骤、也没有覆盖它的离线打包步骤，
# 因此该占位页在交付链上是**原样直达**的 —— 这正是本守卫存在的理由。
if grep -Eq 'TEST_ONLY placeholder|replaced by CI build|<title>[^<]*placeholder|placeholder</title>' "${TARGET}"; then
    fail "交付物是占位前端 —— 拒绝继续（fail-closed）：${TARGET}
现象：本文件命中占位页特征串（TEST_ONLY placeholder / replaced by CI build / placeholder 标题）。
后果：占位页注释自称「会被 CI / build-offline-bundle.sh 覆盖」，但二者都没有覆盖逻辑
     （CI 无前端构建步骤，build-offline-bundle.sh 未包含前端构建）。因此这个占位页会原样
     经 Dockerfile:98 与 Dockerfile:163 进入镜像 /srv/web-console/，交付的是一个假界面：
     CI 绿、构建绿、日志无异常，只有打开页面才会发现 —— 属交付事故。
修复：构建真实产物并覆盖本目录，例如
     cd web-console && npm ci && npm run build      # build = vue-tsc --noEmit && vite build
     # 产物目录为 frontends/web-console/dist，拷贝到 deploy/docker/context/web-console-dist/
     本目录只允许存放真实前端产物，占位页必须消失。"
fi

log "通过：${TARGET} 存在，且不是占位页（判定为真实前端产物）。"
exit 0
