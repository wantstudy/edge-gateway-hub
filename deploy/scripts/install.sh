#!/usr/bin/env bash
# =============================================================================
# iot-daq 网关 · 宿主机一键安装（task 60）
# =============================================================================
# 目标：在一台干净的现场 Linux 宿主上，从离线包完成安装并启动容器。
#
# 设计依据：
#   docs/design/container-deploy.md §3        （离线现场一键安装流程）
#   docs/design/container-machine-binding.md §1 §3（宿主锚点 + 降级签名指纹文件）
#   docs/design/container-persistence-layout.md §1（持久卷布局）
#   docs/design/container-supply-chain.md §7  （离线包结构 + verify.sh）
#
# ★ 陷阱 1：本脚本在**宿主**采集 MAC（排除虚拟/veth 接口），写入签名指纹文件，
#   并以只读挂载给容器；容器绝不自行采集。
# ★ 陷阱 2：本脚本创建宿主持久卷目录与权限，并把授权/试用状态落位固化在卷上。
#
# 用法：
#   sudo ./install.sh --bundle /opt/iot-daq/offline/iot-daq-offline-v1.0.0-amd64 \
#                     [--env-file /opt/iot-daq/.env] \
#                     [--data-dir /var/lib/iot-daq] \
#                     [--skip-verify]      # 不推荐；仅应急，会记入残余风险
#   sudo ./install.sh --help
# =============================================================================

set -euo pipefail

# -----------------------------------------------------------------------------
# 全局常量
# -----------------------------------------------------------------------------
readonly SCRIPT_NAME="$(basename "$0")"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_REV="2026-09-23"

# 默认值（可被参数 / 环境变量覆盖，与 deploy/.env.example 对齐）
DATA_DIR_DEFAULT="/var/lib/iot-daq"
INSTALL_ROOT_DEFAULT="/opt/iot-daq"
CONTAINER_NAME_DEFAULT="iot-daq-gateway"
SERVICE_UID_DEFAULT="990"
SERVICE_GID_DEFAULT="990"
FINGERPRINT_KEY_FILE_DEFAULT="/run/secrets/iot-daq-fingerprint-key"

# 运行期变量
BUNDLE_DIR=""
ENV_FILE=""
DATA_DIR="${IOT_DAQ_DATA_DIR:-${DATA_DIR_DEFAULT}}"
INSTALL_ROOT="${IOT_DAQ_INSTALL_ROOT:-${INSTALL_ROOT_DEFAULT}}"
CONTAINER_NAME="${IOT_DAQ_CONTAINER_NAME:-${CONTAINER_NAME_DEFAULT}}"
SERVICE_UID="${IOT_DAQ_DATA_OWNER_UID:-${SERVICE_UID_DEFAULT}}"
SERVICE_GID="${IOT_DAQ_DATA_OWNER_GID:-${SERVICE_GID_DEFAULT}}"
SKIP_VERIFY=0
VERIFY_DEGRADED=0

# 离线模式默认开启（现场无外网）；置 0 才允许联网动作
OFFLINE_MODE="${IOT_DAQ_OFFLINE_MODE:-1}"

# 独占锁：防并发安装把持久卷权限 / 容器状态改乱
readonly LOCK_FILE="/var/lock/iot-daq-install.lock"

# -----------------------------------------------------------------------------
# 输出与失败辅助
# -----------------------------------------------------------------------------
log()  { printf '[install] %s\n' "$*"; }
warn() { printf '[install][警告] %s\n' "$*" >&2; }
die()  { printf '[install][致命] %s\n' "$*" >&2; exit 1; }

# 三段式失败：现象 / 原因 / 修复
die_full() {
    local code="$1"; shift
    local what="$1"; shift
    local why="$1"; shift
    local fix="$1"; shift
    {
        printf '[install][致命] %s\n' "${what}"
        printf '[install][致命]   原因：%s\n' "${why}"
        printf '[install][致命]   修复：%s\n' "${fix}"
    } >&2
    exit "${code}"
}

usage() {
    cat <<'EOF'
iot-daq 网关宿主机安装脚本

用法：
  install.sh --bundle <离线包目录> [选项]

必选：
  --bundle <dir>      离线包解包目录（含 images/ docker-compose.yml checksums.sha256
                      verify.sh host-fingerprint-collect.sh）。

可选：
  --env-file <file>   环境变量文件（默认 <bundle>/../.env 或 ./deploy/.env）
  --data-dir <dir>    宿主持久卷根目录（默认 /var/lib/iot-daq）
  --install-root <d>  安装根目录（默认 /opt/iot-daq）
  --name <name>       容器名（默认 iot-daq-gateway）
  --uid <uid>         持久卷属主 UID（默认 990）
  --gid <gid>         持久卷属主 GID（默认 990）
  --skip-verify       跳过供应链校验（不推荐，会记入安装残余风险）
  --online            允许联网动作（默认离线，只做本地校验）
  -h, --help          显示本帮助

示例（离线现场）：
  sudo ./install.sh --bundle /opt/iot-daq/offline/iot-daq-offline-v1.0.0-amd64

后续：
  docker compose -f /opt/iot-daq/docker-compose.yml --env-file /opt/iot-daq/.env ps
  curl -fsS http://127.0.0.1:8080/healthz
EOF
}

# -----------------------------------------------------------------------------
# 参数解析
# -----------------------------------------------------------------------------
parse_args() {
    [[ $# -eq 0 ]] && { usage; die "缺少参数：--bundle"; }

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --bundle)       BUNDLE_DIR="${2:-}"; shift 2 ;;
            --bundle=*)     BUNDLE_DIR="${1#*=}"; shift ;;
            --env-file)     ENV_FILE="${2:-}"; shift 2 ;;
            --env-file=*)   ENV_FILE="${1#*=}"; shift ;;
            --data-dir)     DATA_DIR="${2:-}"; shift 2 ;;
            --data-dir=*)   DATA_DIR="${1#*=}"; shift ;;
            --install-root) INSTALL_ROOT="${2:-}"; shift 2 ;;
            --install-root=*) INSTALL_ROOT="${1#*=}"; shift ;;
            --name)         CONTAINER_NAME="${2:-}"; shift 2 ;;
            --name=*)       CONTAINER_NAME="${1#*=}"; shift ;;
            --uid)          SERVICE_UID="${2:-}"; shift 2 ;;
            --uid=*)        SERVICE_UID="${1#*=}"; shift ;;
            --gid)          SERVICE_GID="${2:-}"; shift 2 ;;
            --gid=*)        SERVICE_GID="${1#*=}"; shift ;;
            --skip-verify)  SKIP_VERIFY=1; shift ;;
            --online)       OFFLINE_MODE=0; shift ;;
            -h|--help)      usage; exit 0 ;;
            *)              usage; die "未知参数：$1" ;;
        esac
    done

    [[ -n "${BUNDLE_DIR}" ]] || { usage; die "必须指定 --bundle"; }
}

# 校验路径非空 / 非根 —— 所有递归 chown/chmod/rm 前的硬闸门
assert_safe_path() {
    local p="$1"
    local label="${2:-路径}"
    [[ -n "${p}" ]] || die_full 90 "${label}为空字符串。" "参数解析异常。" "检查传入参数。"
    [[ "${p}" != "/" ]] || die_full 90 "${label}为根目录 /。" "拒绝在 / 上执行变更操作。" "指定具体子目录。"
    case "${p}" in
        /|/bin|/boot|/dev|/etc|/home|/lib|/lib64|/proc|/root|/run|/sbin|/srv|/sys|/tmp|/usr|/var)
            die_full 90 "${label}位于系统关键目录：${p}" "拒绝在系统目录上执行变更操作。" "指定 /var/lib 或 /opt 下的自有子目录。" ;;
    esac
    # 路径深度至少 2 层（如 /var/lib）
    local depth
    depth="$(printf '%s' "${p}" | tr -cd '/' | wc -c | tr -d '[:space:]')"
    [[ "${depth}" -ge 2 ]] || die_full 90 "${label}层级过浅：${p}" "防止误操作上层目录。" "使用至少两层路径，如 /var/lib/iot-daq。"
}

require_root() {
    [[ "$(id -u)" -eq 0 ]] || die_full 91 "需要 root（当前 uid=$(id -u)）。" \
        "安装需创建宿主目录、设置属主、调用 docker。" \
        "使用 sudo 重新执行：sudo ${SCRIPT_NAME} $*"
}

require_cmd() {
    local c="$1"
    command -v "${c}" >/dev/null 2>&1 || die_full 92 "缺少命令：${c}" \
        "宿主未安装该工具或不在 PATH。" \
        "请先安装（如 apt-get install -y ${c}），或从离线包 tools/ 目录拷贝。"
}

# -----------------------------------------------------------------------------
# 步骤 1：环境与前置检查
# -----------------------------------------------------------------------------
step1_precheck() {
    log "步骤 1/9 前置检查"
    require_root "$@"

    [[ -d "${BUNDLE_DIR}" ]] || die_full 10 "离线包目录不存在：${BUNDLE_DIR}" \
        "包未解压或路径写错。" \
        "先在宿主解压：tar -xzf iot-daq-offline-vX.Y.Z-<arch>.tar.gz -C /opt/iot-daq/offline/"

    require_cmd docker
    # compose v2 为插件形态：docker compose
    if ! docker compose version >/dev/null 2>&1; then
        die_full 11 "docker compose（v2 插件）不可用。" \
            "宿主仅装了旧版 docker-compose，或未装 compose 插件。" \
            "安装 docker-compose-plugin，或在离线包 tools/ 内提供该插件。"
    fi

    # 独占锁
    exec 9>"${LOCK_FILE}" || die_full 12 "无法打开锁文件 ${LOCK_FILE}" "权限或磁盘异常。" "检查 /var/lock 可写。"
    if ! flock -n 9; then
        die_full 13 "另一安装进程正在运行（${LOCK_FILE} 被占用）。" \
            "并发安装会破坏持久卷权限与容器状态。" "等待其结束后重试；确认无残留用 flock -u 或删除锁文件。"
    fi

    # 架构识别（离线包按架构分发）
    local arch_raw arch
    arch_raw="$(uname -m)"
    case "${arch_raw}" in
        x86_64|amd64) arch="amd64" ;;
        aarch64|arm64) arch="arm64" ;;
        *) die_full 14 "不支持的宿主架构：${arch_raw}" \
               "设计仅支持 amd64 / arm64（container-supply-chain.md §2.1）。" \
               "请联系厂商确认交付架构。" ;;
    esac
    log "步骤 1/9 OK —— 宿主架构 ${arch}，docker compose 可用，已取得安装锁。"
    printf '%s' "${arch}" > "${BUNDLE_DIR}/.detected-arch" 2>/dev/null || true
}

# -----------------------------------------------------------------------------
# 步骤 2：供应链校验（哈希 + load + cosign 验签）
# -----------------------------------------------------------------------------
step2_supply_chain() {
    log "步骤 2/9 供应链校验"

    if [[ "${SKIP_VERIFY}" -eq 1 ]]; then
        warn "已按 --skip-verify 跳过供应链校验 —— 该决定将记入安装残余风险，请手工核对镜像 digest。"
        return 0
    fi

    if [[ -x "${BUNDLE_DIR}/verify.sh" ]]; then
        log "调用离线包内 verify.sh（唯一实现，与 CI 出厂门禁同源）"
        # verify.sh 内部已 set -euo pipefail 并做硬失败退出
        ( cd "${BUNDLE_DIR}" && ./verify.sh )
        log "步骤 2/9 OK —— verify.sh 通过（哈希 + docker load + cosign 验签）。"
        return 0
    fi

    # 退路：自行执行等价三步
    log "离线包内无 verify.sh，执行等价校验三步"
    if [[ -f "${BUNDLE_DIR}/checksums.sha256" ]]; then
        ( cd "${BUNDLE_DIR}" && sha256sum -c checksums.sha256 ) \
            || die_full 20 "哈希清单校验失败。" \
                 "离线包在传输中被损坏或被篡改。" \
                 "整包作废，从厂商渠道重新获取（禁止跳过校验继续安装）。"
        log "  [1/3] 哈希清单 OK"
    else
        die_full 21 "缺少 checksums.sha256。" "离线包结构不完整。" "重新获取完整离线包。"
    fi

    local img_tar
    img_tar="$(find "${BUNDLE_DIR}/images" -maxdepth 1 -name 'iot-daq-gateway_*.tar' -print -quit 2>/dev/null || true)"
    [[ -n "${img_tar}" ]] || die_full 22 "images/ 下未找到镜像 tar。" \
        "离线包结构不完整或按架构错发。" "确认包名中的架构与宿主一致。"
    log "  [2/3] 导入镜像：${img_tar}"
    docker load -i "${img_tar}" || die_full 23 "docker load 失败：${img_tar}" \
        "镜像层损坏 / 磁盘空间不足。" "检查 df -h 与镜像 tar 完整性后重试。"

    if command -v cosign >/dev/null 2>&1; then
        local pubkey image_ref
        pubkey="${BUNDLE_DIR}/cosign/iot-daq-supply.pub"
        [[ -f "${pubkey}" ]] || die_full 24 "缺少验签公钥 ${pubkey}" \
            "离线包结构不完整。" "重新获取完整离线包（公钥需与安装介质分渠道核对指纹）。"
        image_ref="$(grep -E '^\s*image:' "${BUNDLE_DIR}/docker-compose.yml" | head -1 \
                     | sed -E 's/^\s*image:\s*//; s/\$\{[^}]*\}//' | tr -d '"' | tr -d "'" \
                     | sed -E 's/^[^@]*/iot-daq\/iot-daq-gateway/')"
        log "  [3/3] cosign 验签：${image_ref}"
        cosign verify --key "${pubkey}" "${image_ref}" \
            || die_full 25 "cosign 验签失败。" \
                 "镜像签名无效或镜像与签名不匹配（可能被替换）。" \
                 "拒绝安装；整包作废并从厂商渠道重新获取。"
    else
        if [[ "${IOT_DAQ_ALLOW_VERIFY_DEGRADE:-0}" == "1" ]]; then
            VERIFY_DEGRADED=1
            warn "[3/3] 宿主无 cosign，按 IOT_DAQ_ALLOW_VERIFY_DEGRADE=1 走降级路径："
            warn "      仅完成 [1/3] 哈希 + [2/3] load；**未完成验签**。"
            warn "      请人工核对镜像 digest 与厂商公布值一致，并把该残余风险记入安装单。"
        else
            die_full 26 "宿主无 cosign 且未允许降级。" \
                "无法完成供应链验签（container-supply-chain.md §4.3 要求三步串联）。" \
                "从离线包 tools/cosign 安装 cosign；确需降级时显式置 IOT_DAQ_ALLOW_VERIFY_DEGRADE=1 并自担风险。"
        fi
    fi
    log "步骤 2/9 OK。"
}

# -----------------------------------------------------------------------------
# 步骤 3：准备安装目录（安装根 + 持久卷）
# -----------------------------------------------------------------------------
step3_prepare_dirs() {
    log "步骤 3/9 准备安装目录与持久卷"

    assert_safe_path "${INSTALL_ROOT}" "安装根目录"
    assert_safe_path "${DATA_DIR}" "持久卷根目录"

    mkdir -p "${INSTALL_ROOT}"
    chmod 750 "${INSTALL_ROOT}"

    # ★ 陷阱 2：授权 / 试用 / 队列 / 配置 / 日志五类全部落宿主持久卷
    #   （container-persistence-layout.md §1）
    local sub
    for sub in config logs license trial; do
        mkdir -p "${DATA_DIR}/${sub}"
    done

    # 权限：仅服务账户可写；其他用户只读（machine-fingerprint.md §4）
    chown -R "${SERVICE_UID}:${SERVICE_GID}" "${DATA_DIR}"
    chmod 750 "${DATA_DIR}"
    chmod 750 "${DATA_DIR}/config" "${DATA_DIR}/logs" "${DATA_DIR}/license" "${DATA_DIR}/trial"
    # 授权 / 试用目录再收紧一档
    chmod 700 "${DATA_DIR}/license" "${DATA_DIR}/trial"

    # 安装文件落位
    install -m 0644 "${BUNDLE_DIR}/docker-compose.yml" "${INSTALL_ROOT}/docker-compose.yml"
    if [[ -d "${BUNDLE_DIR}/sbom" ]]; then
        mkdir -p "${INSTALL_ROOT}/sbom"
        cp -f "${BUNDLE_DIR}/sbom/"* "${INSTALL_ROOT}/sbom/" 2>/dev/null || true
    fi
    if [[ -d "${BUNDLE_DIR}/cosign" ]]; then
        mkdir -p "${INSTALL_ROOT}/cosign"
        cp -f "${BUNDLE_DIR}/cosign/"* "${INSTALL_ROOT}/cosign/" 2>/dev/null || true
    fi

    log "步骤 3/9 OK —— 持久卷 ${DATA_DIR} 就位（config/logs/license/trial，属主 ${SERVICE_UID}:${SERVICE_GID}）。"
}

# -----------------------------------------------------------------------------
# 步骤 4：★ 陷阱 1 —— 采集宿主指纹（含宿主 MAC）
# -----------------------------------------------------------------------------
step4_host_fingerprint() {
    log "步骤 4/9 采集宿主机器指纹（陷阱 1 防线）"

    # 宿主 machine-id 必须可读（否则走降级）
    local have_machine_id=0
    if [[ -r /etc/machine-id ]]; then
        have_machine_id=1
        log "  宿主 /etc/machine-id 可读（前 8 位：$(tr -d '[:space:]' < /etc/machine-id | cut -c1-8)…）"
    else
        warn "宿主 /etc/machine-id 不可读（精简宿主），将走签名指纹文件降级路径。"
    fi

    # 宿主 DMI 检查（product_uuid / product_serial / board_serial 三项）
    local dmi_hits=0
    for f in product_uuid product_serial board_serial; do
        if [[ -r "/sys/class/dmi/id/${f}" ]]; then
            dmi_hits=$((dmi_hits + 1))
        fi
    done
    log "  宿主 DMI 可用项：${dmi_hits}（/sys/class/dmi/id/{product_uuid,product_serial,board_serial}）"

    # ★ 宿主 MAC 采集：排除虚拟 / veth / docker / 桥 / lo / 隧道接口
    local host_mac="" iface=""
    if command -v ip >/dev/null 2>&1; then
        while IFS= read -r line; do
            iface="${line%%:*}"
            # 排除常见虚拟接口命名
            case "${iface}" in
                lo|docker*|veth*|br-*|virbr*|vmnet*|tap*|tun*|bond*|dummy*|zt*|wg*|flannel*|cni*|kube*)
                    continue ;;
            esac
            # 排除无物理 device 软链的接口（虚拟接口无 /sys/class/net/<if>/device）
            [[ -e "/sys/class/net/${iface}/device" ]] || continue
            # 排除全零 MAC
            local mac
            mac="$(tr -d '[:space:]' < "/sys/class/net/${iface}/address" 2>/dev/null || true)"
            [[ -n "${mac}" && "${mac}" != "00:00:00:00:00:00" ]] || continue
            host_mac="${mac}"
            break
        done < <(ip -o link show 2>/dev/null | sed -E 's/^[0-9]+:\s+([^:@]+).*/\1/')
    fi

    # 退路：用 /sys/class/net 直接枚举
    if [[ -z "${host_mac}" && -d /sys/class/net ]]; then
        local d
        for d in /sys/class/net/*; do
            iface="$(basename "${d}")"
            case "${iface}" in
                lo|docker*|veth*|br-*|virbr*|vmnet*|tap*|tun*|bond*|dummy*|zt*|wg*) continue ;;
            esac
            [[ -e "${d}/device" ]] || continue
            local mac
            mac="$(tr -d '[:space:]' < "${d}/address" 2>/dev/null || true)"
            [[ -n "${mac}" && "${mac}" != "00:00:00:00:00:00" ]] || continue
            host_mac="${mac}"
            break
        done
    fi

    if [[ -z "${host_mac}" ]]; then
        die_full 30 "无法在宿主采集到物理网卡 MAC。" \
            "宿主可能只有虚拟接口，或 /sys/class/net 不可访问。" \
            "请提供现场网卡信息后人工确认：ip -o link show；\
若确属无物理网卡的虚拟化宿主，联系厂商评估锚点方案（不在现场造数据）。"
    fi
    log "  宿主首个物理网卡：${iface} → MAC ${host_mac}"

    # 生成签名指纹文件（由 host-fingerprint-collect.sh 承担；此处调用以保持单一实现）
    local collector="${BUNDLE_DIR}/host-fingerprint-collect.sh"
    if [[ ! -x "${collector}" && -f "${collector}" ]]; then
        chmod +x "${collector}" 2>/dev/null || true
    fi
    if [[ -x "${collector}" ]]; then
        IOT_DAQ_FINGERPRINT_KEY_FILE="${FINGERPRINT_KEY_FILE_DEFAULT}" \
            "${collector}" --out "${DATA_DIR}" --mac "${host_mac}" \
            || die_full 31 "宿主指纹采集脚本失败。" \
                 "签名盐缺失 / 目录不可写 / 脚本内部校验未过。" \
                 "按脚本输出提示修复（通常是先注入指纹盐 secret）后重试。"
    else
        warn "离线包内无 host-fingerprint-collect.sh —— 退化为纯 machine-id + DMI 锚点。"
        warn "该状态下若宿主为精简内核（无 machine-id 无 DMI），容器启动时会被 entrypoint 拒绝。"
    fi

    # 权限：指纹文件只读语义（容器以 :ro 挂载），宿主侧 0640
    if [[ -f "${DATA_DIR}/host-fingerprint.json" ]]; then
        chown "${SERVICE_UID}:${SERVICE_GID}" "${DATA_DIR}/host-fingerprint.json"
        chmod 640 "${DATA_DIR}/host-fingerprint.json"
        log "  指纹文件：${DATA_DIR}/host-fingerprint.json（0640 ${SERVICE_UID}:${SERVICE_GID}）"
    fi

    # 宿主锚点数量自检：至少 1 个可用（machine-id / DMI / MAC 三者至少一）
    if [[ "${have_machine_id}" -eq 0 && "${dmi_hits}" -eq 0 && -z "${host_mac}" ]]; then
        die_full 32 "宿主锚点全空。" \
            "machine-id 不可读、DMI 无可用项、MAC 采集失败三者同时发生。" \
            "停止安装，回厂核对宿主型号；**禁止**在现场造数据（container-deploy.md §3.2）。"
    fi

    # 导出供后续步骤使用
    HOST_MAC="${host_mac}"
    log "步骤 4/9 OK —— 宿主锚点：machine-id=${have_machine_id} DMI=${dmi_hits} MAC=1。"
}

# -----------------------------------------------------------------------------
# 步骤 5：生成 / 补全 .env（含宿主 MAC）
# -----------------------------------------------------------------------------
step5_env_file() {
    log "步骤 5/9 生成环境变量文件"

    if [[ -z "${ENV_FILE}" ]]; then
        ENV_FILE="${INSTALL_ROOT}/.env"
    fi

    if [[ -f "${ENV_FILE}" ]]; then
        log "  已存在 ${ENV_FILE} —— 仅补全缺失项，不覆盖现场已填值。"
    else
        if [[ -f "${SCRIPT_DIR}/../.env.example" ]]; then
            cp -f "${SCRIPT_DIR}/../.env.example" "${ENV_FILE}"
        elif [[ -f "${BUNDLE_DIR}/.env.example" ]]; then
            cp -f "${BUNDLE_DIR}/.env.example" "${ENV_FILE}"
        else
            # 极简兜底：至少保证关键项存在
            : > "${ENV_FILE}"
        fi
        log "  已由 .env.example 生成 ${ENV_FILE}"
    fi
    chmod 600 "${ENV_FILE}"

    # 幂等地设置 key=value
    set_env_kv() {
        local k="$1" v="$2"
        if grep -qE "^${k}=" "${ENV_FILE}"; then
            # 用 | 作分隔符，避免值中 / 与 & 干扰
            sed -i "s|^${k}=.*|${k}=${v}|" "${ENV_FILE}"
        else
            printf '%s=%s\n' "${k}" "${v}" >> "${ENV_FILE}"
        fi
    }

    set_env_kv "IOT_DAQ_DATA_DIR" "${DATA_DIR}"
    set_env_kv "IOT_DAQ_CONTAINER_NAME" "${CONTAINER_NAME}"
    set_env_kv "IOT_DAQ_DATA_OWNER_UID" "${SERVICE_UID}"
    set_env_kv "IOT_DAQ_DATA_OWNER_GID" "${SERVICE_GID}"
    set_env_kv "IOT_DAQ_FINGERPRINT_FILE" "${DATA_DIR}/host-fingerprint.json"
    # ★ 陷阱 1：宿主 MAC 注入（daemon EnvAnchor 读取）
    set_env_kv "IOT_DAQ_HOST_MAC" "${HOST_MAC:-}"
    set_env_kv "IOT_DAQ_HOST_MACHINE_ID" "/etc/machine-id"
    set_env_kv "IOT_DAQ_HOST_DMI_DIR" "/sys/class/dmi/id"
    set_env_kv "IOT_DAQ_OFFLINE_MODE" "${OFFLINE_MODE}"

    # 镜像 digest 从离线包 compose 抄过来（保持 compose 为唯一事实源）
    local img
    img="$(grep -E '^\s*image:' "${INSTALL_ROOT}/docker-compose.yml" | head -1 \
           | sed -E 's/^\s*image:\s*//' | tr -d '"' | tr -d "'" || true)"
    if [[ -n "${img}" && "${img}" != *'${'* ]]; then
        set_env_kv "IOT_DAQ_IMAGE" "${img}"
    else
        warn "未能从 compose 解析出固定镜像引用（可能是 \${IOT_DAQ_IMAGE} 占位）。"
        warn "请手工确认 ${ENV_FILE} 中 IOT_DAQ_IMAGE 为 digest 形式（禁止 latest / 可变 tag）。"
    fi

    log "步骤 5/9 OK —— ${ENV_FILE}（0600）"
}

# -----------------------------------------------------------------------------
# 步骤 6：校验镜像引用为 digest 形式（红线：禁可变 tag）
# -----------------------------------------------------------------------------
step6_assert_digest() {
    log "步骤 6/9 校验镜像引用为 digest 固定"

    local compose="${INSTALL_ROOT}/docker-compose.yml"
    local img_line
    img_line="$(grep -E '^\s*image:' "${compose}" | head -1 || true)"
    [[ -n "${img_line}" ]] || die_full 40 "compose 中未找到 image 行。" \
        "compose 文件异常。" "重新从离线包安装 docker-compose.yml。"

    # 解析出实际值（可能在 .env 中）
    local resolved="${img_line#*image:}"
    resolved="$(printf '%s' "${resolved}" | sed -E 's/^\s+//' | tr -d '"' | tr -d "'")"

    if [[ "${resolved}" == *'${IOT_DAQ_IMAGE'* ]]; then
        resolved="$(grep -E '^IOT_DAQ_IMAGE=' "${ENV_FILE}" | head -1 | cut -d= -f2- || true)"
    fi

    if [[ "${resolved}" == *":latest"* ]]; then
        die_full 41 "镜像引用含 :latest（红线禁止）。" \
            "离线包被错误打包（CI 未注入 digest）。" \
            "从厂商渠道重新获取按 digest 注入的离线包。"
    fi
    if [[ "${resolved}" != *"@sha256:"* ]]; then
        die_full 42 "镜像引用非 digest 形式：${resolved}" \
            "现场可被替换为同 tag 其他镜像（container-supply-chain.md §6）。" \
            "要求厂商提供 digest 注入版离线包；或由管理员依据发布记录手工改写为 @sha256:<64hex>。"
    fi
    # digest 长度 64 位十六进制
    local digest
    digest="${resolved##*@sha256:}"
    if [[ ! "${digest}" =~ ^[0-9a-fA-F]{64}$ ]]; then
        die_full 43 "digest 格式非法：${digest}" \
            "须为 64 位十六进制。" "核对发布记录中的 digest 后修正。"
    fi
    log "步骤 6/9 OK —— 镜像 ${resolved%%@*}@sha256:${digest:0:12}…（digest 固定）"
}

# -----------------------------------------------------------------------------
# 步骤 7：compose 配置合法性预检
# -----------------------------------------------------------------------------
step7_compose_config() {
    log "步骤 7/9 compose 配置校验"
    ( cd "${INSTALL_ROOT}" && docker compose --env-file "${ENV_FILE}" -f docker-compose.yml config >/dev/null ) \
        || die_full 50 "docker compose config 校验失败。" \
             "compose 文件与 env 组合非法（变量缺失 / 语法错误）。" \
             "手工执行查看详情：cd ${INSTALL_ROOT} && docker compose --env-file ${ENV_FILE} config"
    log "步骤 7/9 OK —— compose 配置合法。"
}

# -----------------------------------------------------------------------------
# 步骤 8：启动容器（幂等：先 down 再 up）
# -----------------------------------------------------------------------------
step8_launch() {
    log "步骤 8/9 启动网关容器"

    ( cd "${INSTALL_ROOT}" \
      && docker compose --env-file "${ENV_FILE}" -f docker-compose.yml up -d --remove-orphans ) \
        || die_full 60 "容器启动失败。" \
             "见下方 docker compose logs。" \
             "执行：cd ${INSTALL_ROOT} && docker compose --env-file ${ENV_FILE} logs --tail=200；\
若日志指向锚点/持久卷校验失败，按 deploy/docker/entrypoint.sh 的提示逐项修复。"

    log "步骤 8/9 OK —— 容器已提交启动。"
}

# -----------------------------------------------------------------------------
# 步骤 9：健康检查（deploy.md §3.1 判据）
# -----------------------------------------------------------------------------
step9_healthcheck() {
    log "步骤 9/9 健康检查（观察 60s 无重启抖动 + /healthz 返回 200）"

    local deadline=$((SECONDS + 60))
    local state=""
    while [[ ${SECONDS} -lt ${deadline} ]]; do
        state="$(docker inspect -f '{{.State.Status}}' "${CONTAINER_NAME}" 2>/dev/null || echo "missing")"
        [[ "${state}" == "running" ]] && break
        sleep 2
    done

    if [[ "${state}" != "running" ]]; then
        {
            printf '[install][致命] 容器未进入 running（当前：%s）。\n' "${state}"
            printf '[install][致命]   日志尾部：\n'
            docker logs --tail=50 "${CONTAINER_NAME}" 2>&1 | sed 's/^/    | /' || true
        } >&2
        die "健康检查失败 —— 见上方日志。禁止以改参数方式绕过（如删指纹文件重生成）。"
    fi

    # 60s 抖动观察窗（restart: unless-stopped 下的重启计数）
    local restarts_before restarts_after
    restarts_before="$(docker inspect -f '{{.RestartCount}}' "${CONTAINER_NAME}" 2>/dev/null || echo 0)"
    sleep 10
    restarts_after="$(docker inspect -f '{{.RestartCount}}' "${CONTAINER_NAME}" 2>/dev/null || echo 0)"
    if [[ "${restarts_after}" -gt "${restarts_before}" ]]; then
        warn "观察到容器重启（RestartCount ${restarts_before} → ${restarts_after}）；请查看日志确认稳定性。"
    fi

    # HTTP 健康端点（宿主侧，host 网络下直接 127.0.0.1）
    local port="${IOT_DAQ_HTTP_PORT:-8080}"
    if command -v curl >/dev/null 2>&1; then
        local body="" code=""
        for _ in 1 2 3 4 5; do
            code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${port}/healthz" || true)"
            if [[ "${code}" == "200" ]]; then
                body="$(curl -fsS "http://127.0.0.1:${port}/healthz" || true)"
                break
            fi
            sleep 3
        done
        if [[ "${code}" == "200" ]]; then
            log "  宿主 curl http://127.0.0.1:${port}/healthz → 200"
            log "  body: ${body}"
        else
            warn "宿主 /healthz 未返回 200（最后一次 http_code=${code}）。"
            warn "管理面可能仍在初始化；请在 1-2 分钟后手工复检。若持续失败见 install 手册排障章节。"
        fi
    else
        warn "宿主无 curl，跳过 /healthz 校验（可用 docker compose ps / logs 人工确认）。"
    fi

    log "步骤 9/9 OK —— 容器 running，健康检查已完成。"
}

# -----------------------------------------------------------------------------
# 安装回执（供现场安装单记录）
# -----------------------------------------------------------------------------
emit_receipt() {
    local receipt="${INSTALL_ROOT}/install-receipt-$(date +%Y%m%d-%H%M%S).txt"
    {
        printf 'iot-daq 网关安装回执\n'
        printf '安装时间      : %s\n' "$(date -Is)"
        printf '脚本版本      : %s (%s)\n' "${SCRIPT_REV}" "${SCRIPT_NAME}"
        printf '宿主架构      : %s\n' "$(uname -m)"
        printf '宿主主机名    : %s\n' "$(hostname)"
        printf '宿主 machine-id(前8): %s\n' "$(tr -d '[:space:]' < /etc/machine-id 2>/dev/null | cut -c1-8 || echo 'N/A')"
        printf '宿主 MAC      : %s\n' "${HOST_MAC:-N/A}"
        printf '持久卷        : %s\n' "${DATA_DIR}"
        printf '安装根        : %s\n' "${INSTALL_ROOT}"
        printf '容器名        : %s\n' "${CONTAINER_NAME}"
        printf '镜像引用      : %s\n' "$(cd "${INSTALL_ROOT}" && grep -E '^\s*image:' docker-compose.yml | head -1 | sed -E 's/^\s*image:\s*//' || true)"
        printf '镜像 digest   : %s\n' "$(docker inspect -f '{{index .RepoDigests 0}}' "${CONTAINER_NAME}" 2>/dev/null || echo 'N/A')"
        printf '供应链校验    : %s\n' "$([[ ${SKIP_VERIFY} -eq 1 ]] && echo '已跳过(残余风险)' || { [[ ${VERIFY_DEGRADED} -eq 1 ]] && echo '降级:未验签(残余风险)' || echo '完整通过(哈希+load+cosign)'; })"
        printf '容器状态      : %s\n' "$(docker inspect -f '{{.State.Status}}' "${CONTAINER_NAME}" 2>/dev/null || echo N/A)"
        printf '\n请将本回执连同宿主指纹值一并抄录到现场安装单（container-deploy.md §3 步骤⑤）。\n'
    } > "${receipt}"
    chmod 644 "${receipt}"
    log "安装回执已生成：${receipt}"
}

# -----------------------------------------------------------------------------
# main
# -----------------------------------------------------------------------------
main() {
    parse_args "$@"

    log "iot-daq 网关安装开始（脚本版本 ${SCRIPT_REV}）"
    log "离线包：${BUNDLE_DIR}"

    step1_precheck "$@"
    step2_supply_chain
    step3_prepare_dirs
    step4_host_fingerprint
    step5_env_file
    step6_assert_digest
    step7_compose_config
    step8_launch
    step9_healthcheck
    emit_receipt

    cat <<EOF

============================================================
iot-daq 网关安装完成
============================================================
安装根      : ${INSTALL_ROOT}
持久卷      : ${DATA_DIR}   ← 授权/试用/租约状态所在（★ 陷阱 2）
env 文件    : ${ENV_FILE}
容器        : ${CONTAINER_NAME}

常用命令：
  查看状态 : docker compose -f ${INSTALL_ROOT}/docker-compose.yml --env-file ${ENV_FILE} ps
  查看日志 : docker compose -f ${INSTALL_ROOT}/docker-compose.yml --env-file ${ENV_FILE} logs -f --tail=200
  健康检查 : curl -fsS http://127.0.0.1:8080/healthz
  停止     : docker compose -f ${INSTALL_ROOT}/docker-compose.yml --env-file ${ENV_FILE} stop
  卸载     : sudo ${SCRIPT_DIR}/uninstall.sh --data-dir ${DATA_DIR}

⚠️ 请勿删除 ${DATA_DIR} —— 它是授权与试用状态的唯一宿主落位。
   docker rm && docker run 不会重置试用（陷阱 2 已由 read_only + 持久卷固化）。

后续建议（本次安装单）：
  1) 记录宿主指纹值与镜像 digest（见上方安装回执文件）；
  2) 配置宿主防火墙，仅放行管理面与协议端口；
  3) 若宿主为 WSL2，请确认 .wslconfig 内存上限足够（≥8GB）后再叠多组件栈。
============================================================
EOF
}

main "$@"
