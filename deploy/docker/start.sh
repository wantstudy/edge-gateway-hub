#!/bin/sh
set -eu

# =============================================================================
# iot-daq 网关容器启动脚本（单容器自足：静态页面 + API 反代 + 业务 daemon）
# =============================================================================
# 进程模型：
#   nginx  后台（对外提供 Web 控制台静态文件 + /api/* 反代到 daemon）
#   daemon 前台（容器 PID 1，保证 SIGTERM 直通、优雅停机）
#
# 端口契约（★ 两者解耦，但反代目标必须与 daemon 实际监听端口同源）：
#   IOT_DAQ_WEB_PORT   对外 Web 端口（用户访问入口，默认 8080）
#   IOT_DAQ_HTTP_PORT  daemon 管理面端口（默认 8081），nginx 反代目标 = 此值
#   历史上曾出现 nginx 写死 8081、daemon 却按 env 监听 9011 的错配
#   → 表现为「页面打得开、所有 API 502」。此处由同一变量渲染即为固化修复。
#
# 配置文件：/etc/nginx/nginx.conf 为**模板**，因容器 read_only 根文件系统
#   （陷阱 2 红线）不可写，故渲染产物落到 tmpfs 的 /tmp/nginx.conf。
# =============================================================================

DAEMON_BIN="/usr/local/bin/iot-daq-daemon"
DAEMON_CONFIG="${IOT_DAQ_CONFIG:-/etc/iot-daq/gateway.toml}"
NGINX_TEMPLATE="/etc/nginx/nginx.conf.template"
NGINX_STATIC="/etc/nginx/nginx.conf"
NGINX_RUNTIME="/tmp/nginx.conf"

# ---- 端口（带默认值，可被 compose/env 覆盖）----
IOT_DAQ_WEB_PORT="${IOT_DAQ_WEB_PORT:-8080}"
IOT_DAQ_HTTP_PORT="${IOT_DAQ_HTTP_PORT:-8081}"
export IOT_DAQ_WEB_PORT IOT_DAQ_HTTP_PORT

echo "[start] 端口: 对外 Web=${IOT_DAQ_WEB_PORT} / daemon=${IOT_DAQ_HTTP_PORT}"

# -----------------------------------------------------------------------------
# 1. 渲染 nginx 配置（模板 → 可写路径）
# -----------------------------------------------------------------------------
if [ -f "$NGINX_TEMPLATE" ]; then
    if command -v envsubst >/dev/null 2>&1; then
        envsubst '${IOT_DAQ_WEB_PORT} ${IOT_DAQ_HTTP_PORT}' \
            < "$NGINX_TEMPLATE" > "$NGINX_RUNTIME"
    else
        # 降级：无 envsubst 时用 sed 精确替换两个占位符
        sed -e "s/\${IOT_DAQ_WEB_PORT}/${IOT_DAQ_WEB_PORT}/g" \
            -e "s/\${IOT_DAQ_HTTP_PORT}/${IOT_DAQ_HTTP_PORT}/g" \
            "$NGINX_TEMPLATE" > "$NGINX_RUNTIME"
    fi
    NGINX_CONF="$NGINX_RUNTIME"
    echo "[start] nginx 配置已渲染: ${NGINX_TEMPLATE} -> ${NGINX_CONF}"
elif [ -f "$NGINX_STATIC" ]; then
    NGINX_CONF="$NGINX_STATIC"
    echo "[start] 未找到模板，回退静态配置 ${NGINX_CONF}（端口不联动，请检查镜像）"
else
    echo "[start] 错误：既无模板 ${NGINX_TEMPLATE} 也无静态配置 ${NGINX_STATIC}"
fi

# -----------------------------------------------------------------------------
# 2. 启动 nginx（失败不阻断 daemon，但给出醒目降级提示）
# -----------------------------------------------------------------------------
NGINX_UP=0
if [ -n "${NGINX_CONF:-}" ] && nginx -t -c "$NGINX_CONF" 2>&1; then
    if nginx -c "$NGINX_CONF" -g 'daemon on;' 2>&1; then
        sleep 1
        if [ -f /var/run/nginx.pid ] && kill -0 "$(cat /var/run/nginx.pid)" 2>/dev/null; then
            NGINX_UP=1
            echo "[start] nginx 已启动 (PID $(cat /var/run/nginx.pid))，监听 ${IOT_DAQ_WEB_PORT}"
        fi
    fi
fi

if [ "$NGINX_UP" -ne 1 ]; then
    echo "[start] [WARNING] nginx 未启动 —— Web 控制台与 /api 反代不可达（daemon 仍将启动）。"
    echo "[start]          常见原因：对外端口 ${IOT_DAQ_WEB_PORT} 已被宿主其它进程占用"
    echo "[start]          （host 网络模式下与宿主共享端口），请换用空闲端口并设置"
    echo "[start]          IOT_DAQ_WEB_PORT 后重启。诊断：nginx -t -c ${NGINX_CONF:-<none>}"
fi

# -----------------------------------------------------------------------------
# 3. 启动 daemon（前台，PID 1）
# -----------------------------------------------------------------------------
echo "[start] 启动 daemon..."
exec "$DAEMON_BIN" --preflight --config "$DAEMON_CONFIG" --foreground
