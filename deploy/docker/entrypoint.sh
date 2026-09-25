#!/usr/bin/env bash
# =============================================================================
# iot-daq 网关容器入口脚本（task 60）
# =============================================================================
# 定位：容器启动前的**宿主锚定 + 持久卷可写性**双重前置校验（fail-fast）。
#
# 与 Dockerfile 的关系：
#   生产镜像用 distroless/static（无 shell），ENTRYPOINT 走 `iot-daq-daemon --preflight`，
#   语义与本脚本**一一对应**。
#   ★ D-15：本脚本**不被 COPY 进** distroless runtime 镜像（无 shell 下不可执行，
#     拷入即死重）；runtime 阶段的入口校验由 daemon `--preflight` 承担。
#   本脚本用于：① deb/rpm + systemd 原生部署 ② 调试镜像（cc/shell 变体）
#   ③ 现场排障时手动执行以定位锚定问题。
#
# ★ 陷阱 1（容器内机器码）：本脚本会在启动前校验「取到的是宿主机 machine-id」，
#   若发现锚点其实是容器内标识 → **拒绝启动**并给出可操作的修复指引。
# ★ 陷阱 2（授权状态落位）：本脚本会校验持久卷可写且授权目录可访问，
#   若 /var/lib/iot-daq 不可写或未挂载 → **拒绝启动**（防止状态悄悄落到可写层）。
# =============================================================================

set -euo pipefail

# -----------------------------------------------------------------------------
# 0. 常量与工具
# -----------------------------------------------------------------------------
readonly SCRIPT_TAG="[iot-daq-entrypoint]"

# 宿主锚点在容器内的只读挂载点（与 docker-compose.yml 严格一致）
readonly HOST_MACHINE_ID_PATH="${IOT_DAQ_HOST_MACHINE_ID_PATH:-/host/etc/machine-id}"
readonly HOST_DMI_DIR_PATH="${IOT_DAQ_HOST_DMI_DIR_PATH:-/host/sys/class/dmi/id}"
readonly HOST_FINGERPRINT_FILE="${IOT_DAQ_HOST_FINGERPRINT_FILE:-/var/lib/iot-daq/host-fingerprint.json}"

# 持久卷根与关键子目录
readonly DATA_DIR="${IOT_DAQ_DATA_DIR:-/var/lib/iot-daq}"
readonly LICENSE_DIR="${DATA_DIR}/license"
readonly TRIAL_DIR="${DATA_DIR}/trial"
readonly CONFIG_DIR="${DATA_DIR}/config"
readonly LOGS_DIR="${DATA_DIR}/logs"

# daemon 可执行文件
readonly DAEMON_BIN="${IOT_DAQ_DAEMON_BIN:-/usr/local/bin/iot-daq-daemon}"

# 失败即打印「现象 + 原因 + 修复动作」三段式，避免只给 exit 1
fail() {
    local code="$1"; shift
    echo "${SCRIPT_TAG} 致命错误：$*" >&2
    echo "${SCRIPT_TAG} 容器拒绝启动（exit ${code}）。请按上方提示修复后重试；" >&2
    echo "${SCRIPT_TAG} 切勿通过删除指纹文件 / 改挂载点的方式绕过（会被运行时自检判为环境变更）。" >&2
    exit "${code}"
}

log() { echo "${SCRIPT_TAG} $*"; }

# -----------------------------------------------------------------------------
# 1. 前置：确认关键路径存在（明确错误信息，而非「no such file」）
# -----------------------------------------------------------------------------
log "1/6 检查宿主锚点挂载是否就位 ..."

if [[ ! -e "${HOST_MACHINE_ID_PATH}" ]]; then
    fail 20 \
"未找到宿主 machine-id 挂载点 ${HOST_MACHINE_ID_PATH}。
   现象：只读挂载缺失或挂成了容器内路径。
   原因：可能未使用 deploy/docker/docker-compose.yml，或手工 docker run 漏了 -v。
   修复：请用随包 compose 启动：
     docker compose --env-file /opt/iot-daq/.env -f /opt/iot-daq/docker-compose.yml up -d
   若为锚点缺失的精简宿主，请先运行 deploy/scripts/host-fingerprint-collect.sh
   生成 ${HOST_FINGERPRINT_FILE} 后再启动（降级路径，见 container-machine-binding.md §3）。"
fi

# -----------------------------------------------------------------------------
# 2. ★ 陷阱 1 核心校验：锚点必须是「宿主」machine-id，不是容器内 machine-id
# -----------------------------------------------------------------------------
log "2/6 校验机器码锚点取自宿主机（陷阱 1）..."

# 2.1 容器内自己的 machine-id（runtime 生成，docker rm 即变）—— 用于比对
CONTAINER_MACHINE_ID=""
if [[ -r /etc/machine-id ]]; then
    CONTAINER_MACHINE_ID="$(tr -d '[:space:]' < /etc/machine-id 2>/dev/null || true)"
fi

HOST_MACHINE_ID=""
if [[ -r "${HOST_MACHINE_ID_PATH}" ]]; then
    HOST_MACHINE_ID="$(tr -d '[:space:]' < "${HOST_MACHINE_ID_PATH}" 2>/dev/null || true)"
fi

# 2.2 若宿主与容器 machine-id 相同 → 说明挂载落回了容器自身（bind 源写错 / 用了容器路径）
if [[ -n "${HOST_MACHINE_ID}" && -n "${CONTAINER_MACHINE_ID}" \
      && "${HOST_MACHINE_ID}" == "${CONTAINER_MACHINE_ID}" ]]; then
    # 说明：这是「很可能把容器内 machine-id 当锚点」的强信号。
    # 容器 runtime 生成的 machine-id 与宿主不同；两者相同意味着 bind 源指向了容器内路径。
    fail 21 \
"检测到锚点 machine-id 与**容器自身** machine-id 相同（值前 8 位：${HOST_MACHINE_ID:0:8}…）。
   现象：${HOST_MACHINE_ID_PATH} 实际挂的是容器内 /etc/machine-id。
   原因（按可能性排序）：
     a) docker run 时 -v 的 src 写成了容器内路径，而非宿主 ${IOT_DAQ_HOST_MACHINE_ID:-/etc/machine-id}；
     b) 使用了旧版 compose 文件（绑定源被改过）；
     c) 现场为精简宿主，无独立 machine-id，需走签名指纹文件降级路径。
   后果（为什么必须拒绝启动，而不是警告）：容器内 machine-id 由 runtime 生成，
     每次 docker rm && docker run 都会变化 → ① 云端判为新设备，授权立即失效；
     ② SQLCipher 本地库（HKDF 由机器码派生密钥）永久解不开。
   修复：改用随包 compose 启动，或执行 deploy/scripts/diagnose-anchors.sh 定位。"
fi

# 2.3 至少要有 1 个可用宿主锚点（machine-id 或 DMI 目录）或签名指纹文件
HOST_DMI_AVAILABLE=0
for candidate in product_uuid product_serial board_serial; do
    if [[ -r "${HOST_DMI_DIR_PATH}/${candidate}" ]]; then
        HOST_DMI_AVAILABLE=$((HOST_DMI_AVAILABLE + 1))
    fi
done

FINGERPRINT_FILE_AVAILABLE=0
if [[ -r "${HOST_FINGERPRINT_FILE}" ]]; then
    FINGERPRINT_FILE_AVAILABLE=1
fi

if [[ -z "${HOST_MACHINE_ID}" && "${HOST_DMI_AVAILABLE}" -eq 0 && "${FINGERPRINT_FILE_AVAILABLE}" -eq 0 ]]; then
    fail 22 \
"宿主锚点全部不可用：machine-id 未挂载、DMI 目录无可用项、签名指纹文件也不存在。
   现象：三个锚点来源（${HOST_MACHINE_ID_PATH}、${HOST_DMI_DIR_PATH}、${HOST_FINGERPRINT_FILE}）全空。
   原因：精简宿主 / 受限内核（无 /sys/class/dmi/id）+ 未执行降级采集。
   修复（按 container-machine-binding.md §3 降级路径）：
     1) 在**宿主**上执行 deploy/scripts/host-fingerprint-collect.sh \
--out ${DATA_DIR}（采集 MAC 等可用宿主信息，产出 HMAC 签名指纹文件）；
     2) 确认 ${HOST_FINGERPRINT_FILE} 已生成且属主/权限正确（0640，属主为服务账户）；
     3) 重新启动容器。
   注意：**禁止**在容器内自行采集（容器内标识一律禁用为锚点）。"
fi

log "2/6 OK —— 宿主 machine-id=${HOST_MACHINE_ID:0:8}…，DMI 可用项=${HOST_DMI_AVAILABLE}，指纹文件=${FINGERPRINT_FILE_AVAILABLE}"

# 2.4 宿主 MAC 必须由安装脚本注入（环境中不得为空）
if [[ -z "${IOT_DAQ_HOST_MAC:-}" ]]; then
    if [[ "${FINGERPRINT_FILE_AVAILABLE}" -eq 1 ]]; then
        log "2/6 提示：IOT_DAQ_HOST_MAC 为空，将走签名指纹文件降级路径（已就位）。"
    else
        fail 23 \
"宿主 MAC 未注入（环境变量 IOT_DAQ_HOST_MAC 为空）且无签名指纹文件可降级。
   现象：宿主首个物理网卡 MAC 缺失。
   原因：未执行安装脚本，或 env 文件未传入容器。
   修复：在**宿主**上执行 deploy/scripts/host-fingerprint-collect.sh，
     它会采集宿主 MAC 并写入 ${HOST_FINGERPRINT_FILE}；
     安装脚本 install.sh 也会把该值写进 deploy/.env 的 IOT_DAQ_HOST_MAC。
   注意：**严禁**回退到容器 veth MAC（重建即变，见 container-machine-binding.md §2）。"
    fi
fi

# 2.5 显式禁止「容器内锚点」开关被打开
if [[ "${IOT_DAQ_ALLOW_CONTAINER_ANCHORS:-0}" != "0" ]]; then
    fail 24 \
"检测到 IOT_DAQ_ALLOW_CONTAINER_ANCHORS=${IOT_DAQ_ALLOW_CONTAINER_ANCHORS}（必须为 0）。
   现象：有人显式打开了「允许容器内锚点」开关。
   原因：该开关在随包、无外网现场中**一律不得开启**。
   修复：从 compose / env-file 中将其设为 0 或直接删除该变量。"
fi

# -----------------------------------------------------------------------------
# 3. ★ 陷阱 2 核心校验：授权状态必须落宿主机持久卷（且不得落镜像可写层）
# -----------------------------------------------------------------------------
log "3/6 校验持久卷与授权目录（陷阱 2）..."

if [[ ! -d "${DATA_DIR}" ]]; then
    fail 30 \
"持久卷根目录 ${DATA_DIR} 不存在。
   现象：宿主目录未创建或未挂载。
   原因：install.sh 未执行，或 compose 卷挂载被删除。
   修复：
     1) sudo mkdir -p ${DATA_DIR}/{config,logs,license,trial}
     2) sudo chown -R ${IOT_DAQ_DATA_OWNER_UID:-990}:${IOT_DAQ_DATA_OWNER_GID:-990} ${DATA_DIR}
     3) sudo chmod 750 ${DATA_DIR}
     4) 确认 compose volumes 中保留 \`${DATA_DIR}:/var/lib/iot-daq:rw\`（★ 陷阱 2 主防线）。"
fi

# 3.1 可写性实测（不能只看权限位：只读挂载同样会写失败）
PROBE_FILE="${DATA_DIR}/.entrypoint-write-probe"
if ! ( : > "${PROBE_FILE}" ) 2>/dev/null; then
    fail 31 \
"持久卷 ${DATA_DIR} 不可写。
   现象：无法创建探针文件 ${PROBE_FILE}。
   原因（按可能性排序）：
     a) 卷以 :ro 挂载（只读挂载下权限位再宽也写不了）；
     b) 属主 / 属组与容器内运行用户（uid 65532）不匹配；
     c) 宿主 SELinux/AppArmor 拒绝（极简排查：ls -ldZ ${DATA_DIR}）。
   后果（为什么必须拒绝启动）：若状态落到容器可写层，
     docker rm && docker run 就是现成的**重置试用绕过路径**。
   修复：
     1) 检查 compose 中该行**没有** :ro；
     2) sudo chown -R 65532:65532 ${DATA_DIR}（或 .env 中统一 UID/GID）；
     3) SELinux 现场：chcon -Rt container_file_t ${DATA_DIR}。"
fi
rm -f "${PROBE_FILE}" 2>/dev/null || true

# 3.2 授权 / 试用 / 配置 / 日志目录必须具备可写性（4 个关键落位，persistence-layout §1）
for d in "${LICENSE_DIR}" "${TRIAL_DIR}" "${CONFIG_DIR}" "${LOGS_DIR}"; do
    if [[ ! -d "${d}" ]]; then
        log "3/6 创建缺失目录 ${d}"
        mkdir -p "${d}" 2>/dev/null || fail 32 \
"无法创建 ${d}。
   修复：在**宿主**上 mkdir -p 该目录并 chown 给容器运行用户后重启。"
    fi
    if ! ( : > "${d}/.w" ) 2>/dev/null; then
        fail 33 \
"授权相关目录 ${d} 不可写。
   现象：授权 / 试用 / 配置 / 日志状态的落位失败。
   原因与修复同上一节（权限 / 只读挂载 / SELinux）。
   红线提醒：这四个目录必须位于宿主持久卷 ${DATA_DIR} 之下（container-persistence-layout.md §1）。"
    fi
    rm -f "${d}/.w" 2>/dev/null || true
done

# 3.3 反向校验：授权目录不得落在容器可写层
# 方法：容器可写层路径下不应存在 license/trial 目录；若存在 → 说明曾用错误配置启动过。
for stray in /home/nonroot/license /home/nonroot/trial /srv/license /srv/trial; do
    if [[ -d "${stray}" ]]; then
        log "3/6 ⚠️ 发现疑似落错位的授权目录 ${stray}（容器可写层）。"
        log "3/6    本脚本不自动删除（可能含唯一副本）。请人工核对后按手册迁移到 ${DATA_DIR}。"
    fi
done

log "3/6 OK —— 持久卷可写，授权/试用/配置/日志四目录就位。"

# -----------------------------------------------------------------------------
# 4. 校验指纹 HMAC 盐 secret 可用（缺失则指纹无法派生）
# -----------------------------------------------------------------------------
log "4/6 校验指纹盐 secret ..."
FINGERPRINT_KEY_FILE="${IOT_DAQ_FINGERPRINT_KEY_FILE:-/run/secrets/iot-daq-fingerprint-key}"
if [[ ! -r "${FINGERPRINT_KEY_FILE}" ]]; then
    fail 40 \
"指纹 HMAC 盐文件不可读：${FINGERPRINT_KEY_FILE}
   现象：无法派生机器指纹 → 授权模块必然启动失败。
   原因：secret 未挂载，或未在安装阶段写入。
   修复（现场交互式注入，勿写进镜像 / 勿提交 git）：
     printf '%s' '<厂商下发的盐>' | sudo tee ${FINGERPRINT_KEY_FILE} >/dev/null
     sudo chmod 600 ${FINGERPRINT_KEY_FILE}
     sudo chown ${IOT_DAQ_DATA_OWNER_UID:-990}:${IOT_DAQ_DATA_OWNER_GID:-990} ${FINGERPRINT_KEY_FILE}
   注意：该文件在 compose 中以 :ro 挂载，宿主侧修改后需重启容器生效。"
fi
# 长度下限校验（防止占位串被当成真盐）
KEY_LEN="$(wc -c < "${FINGERPRINT_KEY_FILE}" | tr -d '[:space:]')"
if [[ "${KEY_LEN}" -lt 16 ]]; then
    fail 41 \
"指纹盐长度不足（${KEY_LEN} 字节 < 16）。
   现象：疑似把占位值（如 REPLACE_ME）写进了盐文件。
   修复：用厂商下发的真实盐替换 ${FINGERPRINT_KEY_FILE} 内容。"
fi
log "4/6 OK —— 指纹盐已就位（长度 ${KEY_LEN} 字节，不予打印内容）。"

# -----------------------------------------------------------------------------
# 5. 校验 daemon 可执行（含完整性自检前置位）
# -----------------------------------------------------------------------------
log "5/6 校验运行时产物 ..."
if [[ ! -x "${DAEMON_BIN}" ]]; then
    fail 50 \
"daemon 可执行文件缺失或不可执行：${DAEMON_BIN}
   现象：镜像层被裁剪 / 挂载覆盖 / 权限被改。
   修复：重新拉取并 load 厂商签名镜像（deploy/scripts/sign-and-verify.sh verify）。"
fi

# -----------------------------------------------------------------------------
# 6. 交接给 daemon
# -----------------------------------------------------------------------------
log "6/6 前置校验全部通过，交棒 daemon（陷阱 1 / 陷阱 2 防线已在启动前生效）。"
log "    宿主 machine-id : ${HOST_MACHINE_ID:0:8}…（来源：宿主机只读挂载）"
log "    持久卷           : ${DATA_DIR}（rw，宿主侧）"
log "    容器可写层授权数据：无（read_only: true）"

exec "${DAEMON_BIN}" \
    --config "${IOT_DAQ_CONFIG:-/etc/iot-daq/gateway.toml}" \
    --foreground \
    "$@"
