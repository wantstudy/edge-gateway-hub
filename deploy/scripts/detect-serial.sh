#!/usr/bin/env bash
# =============================================================================
# iot-daq 网关 · 宿主串口设备探测脚本（task 60 新增，不修改任何既有文件）
# =============================================================================
# 用途：在部署前于宿主上运行，枚举可用串口设备节点，给出建议的
#       IOT_DAQ_SERIAL_DEVICES / IOT_DAQ_SERIAL_GROUP 配置行，
#       供填写 deploy/.env（对应 .env.example 第 97 行「先跑 detect-serial.sh 生成建议配置」）。
#
# 用法：
#   ./detect-serial.sh            # 探测并打印建议配置
#   ./detect-serial.sh --quiet    # 仅打印建议配置行（便于脚本化追加到 .env）
#
# 设计依据：docs/design/container-deploy.md §2.1（串口接入 / 稳定路径 /dev/serial/by-id）
# 衔接说明：本脚本只做「探测 + 建议」，不修改 .env、不动 compose、不触碰容器。
# =============================================================================

set -euo pipefail

readonly SCRIPT_NAME="$(basename "$0")"
QUIET=0

log()  { [[ "${QUIET}" -eq 1 ]] || printf '[detect-serial][info] %s\n' "$*"; }
warn() { printf '[detect-serial][警告] %s\n' "$*" >&2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --quiet) QUIET=1; shift ;;
        --help|-h)
            cat <<EOF
用法：${SCRIPT_NAME} [--quiet]
  --quiet  仅打印建议配置行（便于脚本化写入 .env）
EOF
            exit 0 ;;
        *) echo "未知参数：$1（用 --help 查看用法）" >&2; exit 1 ;;
    esac
done

# -----------------------------------------------------------------------------
# 1. 枚举设备节点
# -----------------------------------------------------------------------------
shopt -s nullglob
tty_all=(/dev/tty*)
tty_usb=(/dev/ttyUSB*)
tty_acm=(/dev/ttyACM*)
tty_s=(/dev/ttyS*)
by_id=(/dev/serial/by-id/*)
shopt -u nullglob

log "枚举串口设备节点 ..."
if [[ ${#tty_usb[@]} -gt 0 ]]; then
    log "  /dev/ttyUSB* (${#tty_usb[@]}): ${tty_usb[*]}"
fi
if [[ ${#tty_acm[@]} -gt 0 ]]; then
    log "  /dev/ttyACM* (${#tty_acm[@]}): ${tty_acm[*]}"
fi
if [[ ${#tty_s[@]} -gt 0 ]]; then
    log "  /dev/ttyS*   (${#tty_s[@]}): ${tty_s[*]}"
fi
if [[ ${#by_id[@]} -gt 0 ]]; then
    log "  /dev/serial/by-id/* (${#by_id[@]}):"
    for d in "${by_id[@]}"; do
        # 解析符号链接指向的真实节点，便于判断归属
        local_real="$(readlink -f "$d" 2>/dev/null || echo "$d")"
        log "      $d -> ${local_real}"
    done
else
    log "  /dev/serial/by-id/*: 无（多设备 / 换口场景强烈建议用稳定 by-id 路径，见下）"
fi

# -----------------------------------------------------------------------------
# 2. 推荐设备：优先 /dev/serial/by-id（换口不漂），否则首个 ttyUSB/ttyACM
# -----------------------------------------------------------------------------
RECOMMENDED=""
if [[ ${#by_id[@]} -gt 0 ]]; then
    RECOMMENDED="${by_id[0]}"
elif [[ ${#tty_usb[@]} -gt 0 ]]; then
    RECOMMENDED="${tty_usb[0]}"
elif [[ ${#tty_acm[@]} -gt 0 ]]; then
    RECOMMENDED="${tty_acm[0]}"
fi

if [[ -z "${RECOMMENDED}" ]]; then
    warn "未检测到任何串口设备节点（/dev/ttyUSB* / ttyACM* / serial/by-id）。"
    warn "若现场确有串口设备，请确认设备已插入、内核驱动已加载；或改用 /dev/ttyS* 物理串口。"
fi

# -----------------------------------------------------------------------------
# 3. 确定串口组（优先 dialout，否则 tty），并打印成员与节点属主
# -----------------------------------------------------------------------------
SERIAL_GROUP=""
if getent group dialout >/dev/null 2>&1; then
    SERIAL_GROUP="dialout"
elif getent group tty >/dev/null 2>&1; then
    SERIAL_GROUP="tty"
else
    warn "未找到 dialout 或 tty 组；compose 的 group_add 将回退到默认 dialout。"
    SERIAL_GROUP="dialout"
fi

log "串口访问组：${SERIAL_GROUP}"
if command -v getent >/dev/null 2>&1; then
    members="$(getent group "${SERIAL_GROUP}" | cut -d: -f4)"
    if [[ -n "${members}" ]]; then
        log "  ${SERIAL_GROUP} 组成员：${members}"
    else
        log "  ${SERIAL_GROUP} 组无额外成员（仅 root 与组内进程可访问串口）。"
    fi
fi

# 设备节点属主（帮助判断当前用户是否已在组内）
for d in "${tty_usb[@]}" "${tty_acm[@]}" "${by_id[@]}"; do
    real="$(readlink -f "$d" 2>/dev/null || echo "$d")"
    if [[ -e "${real}" ]]; then
        owner="$(stat -c '%U:%G' "${real}" 2>/dev/null || echo '?')"
        perms="$(stat -c '%A' "${real}" 2>/dev/null || echo '?')"
        log "  节点 ${d} (${real}) 属主=${owner} 权限=${perms}"
    fi
done

# -----------------------------------------------------------------------------
# 4. 输出建议配置行（与 deploy/.env.example §7 一致）
# -----------------------------------------------------------------------------
echo ""
echo "# ===== 由 detect-serial.sh 生成（$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)） ====="
if [[ -n "${RECOMMENDED}" ]]; then
    echo "IOT_DAQ_SERIAL_DEVICES=${RECOMMENDED}"
else
    echo "IOT_DAQ_SERIAL_DEVICES=/dev/ttyUSB0   # 未探测到设备，保留默认值；确认接线后重跑本脚本"
fi
echo "IOT_DAQ_SERIAL_GROUP=${SERIAL_GROUP}"
echo "# ===== 说明 ====="
if [[ ${#by_id[@]} -gt 1 || ${#tty_usb[@]} -gt 1 ]]; then
    echo "# 检测到多个串口设备。compose 当前仅以单个 IOT_DAQ_SERIAL_DEVICES 挂载一个节点；"
    echo "# 多设备场景需额外声明（需编辑 deploy/docker/docker-compose.yml 的 devices: 列表，超出本脚本范围）。"
fi
if [[ ${#by_id[@]} -gt 0 ]]; then
    echo "# 推荐用 /dev/serial/by-id/* 稳定路径：拔插顺序变化导致 ttyUSB 编号漂移时仍可正确绑定。"
fi
echo "# 将以上两行写入 deploy/.env 对应位置（替换 §7 默认值）即可。"
