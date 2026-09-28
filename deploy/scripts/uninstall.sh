#!/usr/bin/env bash
# =============================================================================
# iot-daq 网关 · 卸载脚本（task 60）
# =============================================================================
# 设计原则（container-persistence-layout.md §1 + container-machine-binding.md）：
#   授权 / 试用 / 租约状态**默认保留** —— 卸载容器不等于放弃授权。
#   删除持久卷是不可逆操作，必须显式指定 --purge-data 且二次确认。
#
# ★ 陷阱 2 相关的安全默认值：
#   - 默认只停容器、保留 /var/lib/iot-daq（含 license/ trial/ 队列/ 配置）。
#   - 只有 --purge-data 才会删除。删除前会做路径安全校验（非空 / 非 / / 非系统目录）。
#   - 删除前会打印将丢失的内容摘要（授权 / 试用 / 未发送队列），要求人工确认。
#
# 用法：
#   sudo ./uninstall.sh                       # 仅停容器，保留数据（默认）
#   sudo ./uninstall.sh --remove-install      # 停容器 + 删除 /opt/iot-daq 安装文件
#   sudo ./uninstall.sh --purge-data --yes-i-know-data-is-lost
#                                            # 危险：连持久卷一起删（需双参数）
#   sudo ./uninstall.sh --help
# =============================================================================

set -euo pipefail

readonly SCRIPT_NAME="$(basename "$0")"
readonly SCRIPT_REV="2026-09-23"

DATA_DIR="${IOT_DAQ_DATA_DIR:-/var/lib/iot-daq}"
INSTALL_ROOT="${IOT_DAQ_INSTALL_ROOT:-/opt/iot-daq}"
ENV_FILE="${IOT_DAQ_ENV_FILE:-${INSTALL_ROOT}/.env}"
CONTAINER_NAME="${IOT_DAQ_CONTAINER_NAME:-iot-daq-gateway}"

REMOVE_INSTALL=0
PURGE_DATA=0
CONFIRMED_PURGE=0
KEEP_IMAGES="${IOT_DAQ_KEEP_IMAGES:-1}"

log()  { printf '[uninstall] %s\n' "$*"; }
warn() { printf '[uninstall][警告] %s\n' "$*" >&2; }
die()  { printf '[uninstall][致命] %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
iot-daq 网关卸载脚本

用法：
  uninstall.sh [选项]

选项：
  --data-dir <dir>                  持久卷根目录（默认 /var/lib/iot-daq）
  --install-root <dir>              安装根目录（默认 /opt/iot-daq）
  --env-file <file>                 环境变量文件（默认 <install-root>/.env）
  --name <container>                容器名（默认 iot-daq-gateway）
  --remove-install                  同时删除 <install-root> 里的安装文件
                                     （compose / env / sbom / cosign 公钥）
  --purge-data                      ★ 危险：删除持久卷（授权 / 试用 / 配置 / 队列全部丢失）
  --yes-i-know-data-is-lost         ★ 与 --purge-data 配对使用的确认参数
  --remove-images                   同时删除网关镜像（默认保留，便于快速恢复）
  -h, --help                        显示本帮助

默认行为（安全）：
  停止并移除容器，**保留**持久卷与安装文件。
  授权 / 试用状态不受影响，重新安装同版本即可恢复服务。

注意：
  删除持久卷后，试用状态会依赖云端「首次激活时间」兜底；
  若同时换了宿主，则视同换机，需走管理后台「废弃 + 重发」。
EOF
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --data-dir)         DATA_DIR="${2:-}"; shift 2 ;;
            --data-dir=*)       DATA_DIR="${1#*=}"; shift ;;
            --install-root)     INSTALL_ROOT="${2:-}"; shift 2 ;;
            --install-root=*)   INSTALL_ROOT="${1#*=}"; shift ;;
            --env-file)         ENV_FILE="${2:-}"; shift 2 ;;
            --env-file=*)       ENV_FILE="${1#*=}"; shift ;;
            --name)             CONTAINER_NAME="${2:-}"; shift 2 ;;
            --name=*)           CONTAINER_NAME="${1#*=}"; shift ;;
            --remove-install)   REMOVE_INSTALL=1; shift ;;
            --purge-data)       PURGE_DATA=1; shift ;;
            --yes-i-know-data-is-lost) CONFIRMED_PURGE=1; shift ;;
            --remove-images)    KEEP_IMAGES=0; shift ;;
            -h|--help)          usage; exit 0 ;;
            *)                  usage; die "未知参数：$1" ;;
        esac
    done
}

# 路径安全闸门：任何 rm -rf 之前的强制校验
assert_safe_rm_path() {
    local p="$1"
    local label="${2:-路径}"

    [[ -n "${p}" ]] || die "拒绝删除：${label}为空字符串。"
    [[ "${p}" != "/" ]] || die "拒绝删除：${label}为根目录 /（会摧毁宿主）。"

    case "${p}" in
        *..*) die "拒绝删除：${label}含 '..' 片段（${p}）；请使用规范化绝对路径。" ;;
    esac
    case "${p}" in
        /*) : ;;
        *)  die "拒绝删除：${label}必须是绝对路径（当前 ${p}）。" ;;
    esac

    case "${p}" in
        /|/bin|/boot|/dev|/etc|/home|/lib|/lib64|/proc|/root|/run|/sbin|/srv|/sys|/tmp|/usr|/var)
            die "拒绝删除：${label}位于系统关键目录（${p}）。" ;;
    esac

    local depth
    depth="$(printf '%s' "${p}" | tr -cd '/' | wc -c | tr -d '[:space:]')"
    [[ "${depth}" -ge 2 ]] || die "拒绝删除：${label}层级过浅（${p}），至少需两层（如 /var/lib/iot-daq）。"

    # 必须包含项目特征目录名，避免手抖传了别的路径
    case "${p}" in
        *iot-daq*|*iot_daq*|*iotdaq*) : ;;
        *) warn "注意：${label}（${p}）不含 'iot-daq' 特征字样，请再次确认这是网关数据目录。" ;;
    esac
}

require_root() {
    [[ "$(id -u)" -eq 0 ]] || die "需要 root（当前 uid=$(id -u)）。请用 sudo 重新执行。"
}

# -----------------------------------------------------------------------------
# 步骤 1：停止并移除容器
# -----------------------------------------------------------------------------
step_stop_container() {
    log "步骤 1/4 停止并移除容器 ${CONTAINER_NAME}"

    if command -v docker >/dev/null 2>&1; then
        if [[ -f "${INSTALL_ROOT}/docker-compose.yml" ]]; then
            local args=(compose -f "${INSTALL_ROOT}/docker-compose.yml")
            [[ -f "${ENV_FILE}" ]] && args=(compose --env-file "${ENV_FILE}" -f "${INSTALL_ROOT}/docker-compose.yml")
            ( cd "${INSTALL_ROOT}" && docker "${args[@]}" down --remove-orphans ) \
                || warn "docker compose down 返回非零；继续尝试直接移除容器。"
        fi

        if docker inspect "${CONTAINER_NAME}" >/dev/null 2>&1; then
            docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || warn "无法移除容器 ${CONTAINER_NAME}（可能已不存在）。"
        fi
        log "  容器已移除。"
    else
        warn "宿主无 docker 命令 —— 跳过容器清理。若为原生（systemd）部署，请执行："
        warn "  systemctl disable --now iot-daq.service"
    fi
}

# -----------------------------------------------------------------------------
# 步骤 2：数据保留决策（★ 默认保留）
# -----------------------------------------------------------------------------
step_decide_data() {
    log "步骤 2/4 授权数据处置决策"

    if [[ ! -d "${DATA_DIR}" ]]; then
        log "  持久卷 ${DATA_DIR} 不存在，无需处置。"
        return 0
    fi

    # 内容摘要：让运维看清「删掉会失去什么」
    local license_n trial_n db_n cfg_n
    license_n="$(find "${DATA_DIR}/license" -mindepth 1 2>/dev/null | wc -l | tr -d '[:space:]')"
    trial_n="$(find "${DATA_DIR}/trial" -mindepth 1 2>/dev/null | wc -l | tr -d '[:space:]')"
    db_n="$(find "${DATA_DIR}" -maxdepth 1 -name '*.db' 2>/dev/null | wc -l | tr -d '[:space:]')"
    cfg_n="$(find "${DATA_DIR}/config" -mindepth 1 2>/dev/null | wc -l | tr -d '[:space:]')"
    local du
    du="$(du -sh "${DATA_DIR}" 2>/dev/null | cut -f1 || echo '未知')"

    cat <<EOF

  持久卷：${DATA_DIR}  （占用 ${du}）
  ├── license/  条目：${license_n}   ← 授权凭证（加密）
  ├── trial/    条目：${trial_n}   ← 试用标记（多重冗余之一）
  ├── *.db      数量：${db_n}   ← 队列库 / 遥测库（未发送数据在此）
  ├── config/   条目：${cfg_n}   ← 点位表 / 北向出口 / 告警规则
  └── logs/                      ← 运行日志
  └── host-fingerprint.json      ← 宿主指纹（可重新生成，见 install.sh 步骤 4）

EOF

    if [[ "${PURGE_DATA}" -eq 0 ]]; then
        log "  【默认行为】保留持久卷 —— 授权 / 试用 / 配置 / 队列全部保留。"
        log "  重新安装同版本即可恢复服务，无需重新激活。"
        return 0
    fi

    # ---- 危险路径 ----
    warn "================================================================"
    warn "--purge-data 已指定：将**永久删除**上述全部内容。"
    warn "后果："
    warn "  - 本地授权凭证与试用标记丢失；"
    warn "  - 试用状态只能依赖云端「首次激活时间」兜底；"
    warn "  - 若同时更换了宿主（machine-id 重置），视同换机，"
    warn "    必须由厂商管理员在总管理后台执行「废弃 + 重发」（客户端无解绑入口）。"
    warn "================================================================"

    if [[ "${CONFIRMED_PURGE}" -ne 1 ]]; then
        die "拒绝执行：缺少确认参数。若确要删除持久卷，请**同时**指定 \
--purge-data --yes-i-know-data-is-lost。"
    fi

    assert_safe_rm_path "${DATA_DIR}" "持久卷根目录"
    log "  路径安全校验通过：${DATA_DIR}"
}

# -----------------------------------------------------------------------------
# 步骤 3：执行删除（仅在显式确认后）
# -----------------------------------------------------------------------------
step_purge() {
    if [[ "${PURGE_DATA}" -eq 0 ]]; then
        return 0
    fi

    if [[ ! -d "${DATA_DIR}" ]]; then
        log "步骤 3/4 持久卷不存在，跳过删除。"
        return 0
    fi

    log "步骤 3/4 删除持久卷（已确认）"

    # 单点兜底：即便路径校验通过，也用 find -maxdepth 1 逐项删，避免 rm -rf 语义失控
    # 保留根目录自身（避免手抖传 /var/lib 时波及宿主其他内容 —— 已由 assert_safe_rm_path 拦住，
    # 此处为第二道防线：只删该目录内部条目）。
    log "  删除内容：${DATA_DIR}/*"
    find "${DATA_DIR}" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} + \
        || die "删除 ${DATA_DIR} 内部条目失败（权限 / 占用）。"

    # 若目录已空则移除空壳
    if [[ -d "${DATA_DIR}" ]] && [[ -z "$(ls -A "${DATA_DIR}" 2>/dev/null)" ]]; then
        rmdir "${DATA_DIR}" 2>/dev/null || true
        log "  已移除空目录 ${DATA_DIR}"
    fi
    log "  持久卷内容已清除。"
}

# -----------------------------------------------------------------------------
# 步骤 4：安装文件与镜像（可选）
# -----------------------------------------------------------------------------
step_remove_install() {
    log "步骤 4/4 清理安装文件"

    if [[ "${REMOVE_INSTALL}" -eq 1 && -d "${INSTALL_ROOT}" ]]; then
        assert_safe_rm_path "${INSTALL_ROOT}" "安装根目录"
        # 逐项删除，保留根目录本身
        local items=(docker-compose.yml .env sbom cosign install-receipt-*.txt)
        local it
        for it in "${items[@]}"; do
            # shellcheck disable=SC2086  # 有意让 glob 展开
            rm -rf -- ${INSTALL_ROOT}/${it} 2>/dev/null || true
        done
        if [[ -z "$(ls -A "${INSTALL_ROOT}" 2>/dev/null)" ]]; then
            rmdir "${INSTALL_ROOT}" 2>/dev/null || true
        fi
        log "  安装文件已从 ${INSTALL_ROOT} 清除。"
    else
        log "  保留安装文件：${INSTALL_ROOT}（未指定 --remove-install）。"
    fi

    if [[ "${KEEP_IMAGES}" -eq 0 ]]; then
        log "  按 --remove-images 删除网关镜像 ..."
        local imgs
        imgs="$(docker images --format '{{.Repository}}:{{.Tag}} {{.ID}}' 2>/dev/null \
                | grep -i 'iot-daq' | awk '{print $2}' | sort -u || true)"
        if [[ -n "${imgs}" ]]; then
            # shellcheck disable=SC2086
            docker rmi -f ${imgs} >/dev/null 2>&1 || warn "部分镜像删除失败（可能被其他容器引用）。"
            log "  镜像已删除。"
        else
            log "  未找到 iot-daq 相关镜像。"
        fi
    else
        log "  保留网关镜像（默认，便于快速恢复；指定 --remove-images 可删除）。"
    fi
}

# -----------------------------------------------------------------------------
# main
# -----------------------------------------------------------------------------
main() {
    parse_args "$@"
    require_root

    log "iot-daq 网关卸载开始（脚本版本 ${SCRIPT_REV}）"
    log "数据目录：${DATA_DIR}"
    log "安装根  ：${INSTALL_ROOT}"

    step_stop_container
    step_decide_data
    step_purge
    step_remove_install

    cat <<EOF

============================================================
iot-daq 网关卸载完成
============================================================
容器        : 已移除
持久卷      : $([[ ${PURGE_DATA} -eq 1 ]] && echo "已按 --purge-data 删除 ${DATA_DIR}" || echo "已保留 ${DATA_DIR}（授权/试用/配置/队列完整）")
安装文件    : $([[ ${REMOVE_INSTALL} -eq 1 ]] && echo "已删除" || echo "已保留 ${INSTALL_ROOT}")
镜像        : $([[ ${KEEP_IMAGES} -eq 1 ]] && echo "已保留" || echo "已删除")

如需重新安装：
  sudo ${INSTALL_ROOT}/scripts/install.sh --bundle <离线包目录>
  # 或从离线包内：
  sudo ./install.sh --bundle <离线包目录> --data-dir ${DATA_DIR}
============================================================
EOF
}

main "$@"
