# =============================================================================
# iot-daq 网关容器镜像 · 多阶段构建（task 60）
# =============================================================================
# 设计依据：
#   docs/design/container-deploy.md            §5 明确不做清单（无 privileged / 无 docker.sock）
#   docs/design/container-supply-chain.md      §3 基础镜像 digest pin（禁可变 tag）
#   docs/design/container-machine-binding.md   §1 宿主锚点只读挂载路径
#   docs/design/container-persistence-layout.md§2 镜像内不含敏感物清单
#   docs/design/installer-hardening.md         §10 镜像 = 分发物，纳入 cosign 体系
#
# 硬约束（违反即视为缺陷）：
#   1) 基础镜像一律 `FROM <image>@sha256:<digest>`，禁 `:latest` / 可变 tag。
#      digest 不手写：由 `deploy/scripts/render-dockerfile-digests.sh` 从
#      `deploy/base-images.lock.yaml` 渲染注入（supply-chain §3.1）。
#
#   ★ 本文件不是「可直接构建物」：下方 FROM 的 digest 段为**占位 token**
#     （__BUILDER_DIGEST__ / __RUNTIME_DIGEST__），保留 `@sha256:` 形式。
#     构建前必须先用 render 脚本渲染出带真实 digest 的 Dockerfile：
#       deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
#       docker buildx build -f deploy/docker/Dockerfile.rendered ...
#     直接 `docker build -f deploy/docker/Dockerfile` 会因占位非法而失败
#     （fail-closed —— 让「未 pin 的构建」不可能静默成功）。
#   2) runtime 阶段只拷二进制（daemon + healthprobe）+ Web 静态资源 + nginx 配置，
#      绝不 COPY 源码、.git、密钥、激活码、宿主指纹（persistence-layout §2）。
#   3) 以非 root 用户运行；容器内进程不申请任何 capability。
#   4) 目标架构 amd64 / arm64（buildx --platform linux/amd64,linux/arm64）。
#   5) fail-closed：产物路径错位必须让构建失败，绝不允许 BuildKit glob 静默产出空层。
#
# 架构变化（2026-09-27）：
#   - 原 distroless/static 改为 nginx:1.25-alpine（容器内置 nginx，不再依赖宿主 nginx）
#   - nginx 默认监听 8080：静态文件 + /api/* → daemon:$IOT_DAQ_HTTP_PORT
#   - daemon 默认监听 8081（容器内部）
#   - 两者均由 env 覆盖且同源联动（IOT_DAQ_WEB_PORT / IOT_DAQ_HTTP_PORT），
#     宿主 8080 被占用时只改前者即可（实测该场景常见：宝塔 / Java 服务占坑）
#   - 入口 /usr/local/bin/start.sh（COPY 自 deploy/docker/start.sh）同时启动 nginx + daemon
#
# 构建示例：
#   deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
#   docker buildx build --platform linux/amd64 \
#     -f deploy/docker/Dockerfile.rendered -t iot-daq-gateway:v1.0.0 --push .
# =============================================================================

# -----------------------------------------------------------------------------
# 阶段 1：builder —— 编译 daemon（Rust workspace 唯一运行体 crate）
# -----------------------------------------------------------------------------
FROM rust:1.88-slim-bookworm@sha256:__BUILDER_DIGEST__ AS builder

ARG TARGETARCH

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends musl-tools; \
    rm -rf /var/lib/apt/lists/*

ENV CARGO_TERM_COLOR=never \
    CARGO_NET_OFFLINE=false \
    RUSTFLAGS="-C strip=symbols"

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates/protocol-proto/Cargo.toml crates/protocol-proto/
COPY crates/daemon/Cargo.toml crates/daemon/
COPY crates/licensing-server/Cargo.toml crates/licensing-server/
RUN set -eux; \
    for d in protocol-proto daemon licensing-server; do \
        mkdir -p "crates/$d/src"; \
        echo '' > "crates/$d/src/lib.rs"; \
    done

RUN cargo build --release --package daemon || true

COPY crates/ ./crates/
COPY deploy/docker/context/web-console-dist/ /build/web-console-dist/
COPY deploy/docker/healthprobe/main.rs /build/healthprobe.rs

RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TRIPLE="x86_64-unknown-linux-musl" ;; \
      arm64) RUST_TRIPLE="aarch64-unknown-linux-musl" ;; \
      *) echo "不支持的 TARGETARCH=$TARGETARCH（仅 amd64 / arm64）" >&2; exit 1 ;; \
    esac; \
    echo "BUILD_TARGET=$RUST_TRIPLE" > /build/build-info.txt; \
    rustup target add "$RUST_TRIPLE"; \
    export RUSTFLAGS="-C strip=symbols -C target-feature=+crt-static"; \
    cargo build --release --package daemon --bin iot-daq-daemon --target "$RUST_TRIPLE" --target-dir /build/target; \
    BIN="/build/target/${RUST_TRIPLE}/release/iot-daq-daemon"; \
    if [ ! -f "$BIN" ]; then \
        echo "FATAL: daemon 二进制不存在：$BIN（产物路径与构建命令错位，fail-closed 拒绝空层）" >&2; \
        exit 1; \
    fi; \
    rustc --target "$RUST_TRIPLE" -C target-feature=+crt-static -C strip=symbols \
        -O /build/healthprobe.rs -o /build/out_healthprobe; \
    test -f /build/out_healthprobe; \
    mkdir -p /build/out; \
    cp "$BIN" /build/out/iot-daq-daemon; \
    cp /build/out_healthprobe /build/out/healthprobe; \
    file /build/out/iot-daq-daemon 2>/dev/null || true

# -----------------------------------------------------------------------------
# 阶段 2：runtime —— nginx:alpine，非 root，容器内置反代
# -----------------------------------------------------------------------------
# nginx:1.25-alpine：轻量级 HTTP 服务器 + 反向代理，替代 distroless/static。
# nginx 负责：
#   1. 静态文件服务（/、/assets/*、favicon/logo）
#   2. API 反代（/api/* → daemon:8081）
#   3. SSE 长连接（proxy_read_timeout 300s）
# daemon 默认监听 8081（容器内部），nginx 默认监听 8080（对外暴露）；
# 实际端口由 IOT_DAQ_HTTP_PORT / IOT_DAQ_WEB_PORT 决定，且 nginx 反代目标与之同源
FROM nginx:1.25-alpine@sha256:__RUNTIME_DIGEST__ AS runtime

ARG TARGETARCH
LABEL org.opencontainers.image.title="iot-daq-gateway" \
      org.opencontainers.image.description="工业边缘数据汇聚与统一分发网关（内置 nginx）" \
      org.opencontainers.image.licenses="LicenseRef-Proprietary" \
      org.opencontainers.image.source="https://vendor-internal.example/iot-daq" \
      com.vendor.iot-daq.arch="${TARGETARCH}" \
      com.vendor.iot-daq.fingerprint-policy="host-only" \
      com.vendor.iot-daq.state-policy="persistent-volume-only"

# ---- 运行时目录契约 ----
# /var/lib/iot-daq 由 compose 挂载宿主目录
# /var/log/nginx 由 compose 挂载或 tmpfs

# ---- 运行身份：容器内固定 uid/gid 65532（与 compose 的 user: 及持久卷属主一致）----
# ★ 必须建【具名】用户 nonroot，不能只依赖裸 uid：
#   nginx `user` 指令用 getpwnam() 按名称解析；写 `user 65532;` 而 /etc/passwd 无同名条目时，
#   以 root 启动会 [emerg] getpwnam("65532") failed，以 65532 启动则该指令被忽略而「看似正常」
#   —— 同一份配置在两种运行身份下表现相反。建名后写 `user nonroot;` 两种情况都成立。
#   另：cap_drop: ALL 下无 CAP_SETUID，身份只能靠运行期 uid 或此处具名用户切换。
RUN set -eux; \
    addgroup -g 65532 nonroot; \
    adduser -D -u 65532 -G nonroot -s /sbin/nologin nonroot

WORKDIR /home/nonroot

# ---- 仅拷贝运行时产物 ----
# daemon 二进制（musl 静态）
COPY --from=builder /build/out/iot-daq-daemon /usr/local/bin/iot-daq-daemon

# healthprobe 静态探针
COPY --from=builder /build/out/healthprobe /usr/local/bin/healthprobe

# Web 控制台静态资源
COPY --from=builder /build/web-console-dist/ /srv/web-console/

# ---- 默认配置模板 ----
COPY deploy/docker/config/gateway.default.toml /etc/iot-daq/gateway.toml

# ---- nginx 配置（**模板**：端口为占位符，由 start.sh 用 envsubst 渲染后加载）----
# 原因：容器 read_only 根文件系统（陷阱 2 红线）下 /etc/nginx 不可写，
# 且对外端口需可在部署时覆盖（宿主常见 8080 冲突）。渲染产物落 /tmp/nginx.conf。
# 端口联动红线见 nginx.conf.template 文件头注释。
COPY deploy/docker/nginx.conf.template /etc/nginx/nginx.conf.template

# mime.types：从 nginx 镜像提取，保证构建可复现
COPY deploy/docker/nginx.mime.types /etc/nginx/mime.types

# ---- 启动脚本：同时启动 nginx + daemon ----
# ★ 必须由仓库文件 deploy/docker/start.sh 提供（唯一事实源），
#   **严禁**在 Dockerfile 内再内联一份副本：内联副本不含 envsubst 渲染，
#   nginx.conf.template 的 ${IOT_DAQ_WEB_PORT} / ${IOT_DAQ_HTTP_PORT} 占位符不会被替换，
#   表现为「模板被 nginx 直接加载而失败」；若回退到写死 8081 的静态配置，
#   则退化为端口错配（页面打得开、所有 /api 502）。
# start.sh 职责：渲染 nginx 模板（/etc/nginx 在 read_only 根下不可写 → 产物落 /tmp/nginx.conf）、
#   后台起 nginx、前台 exec daemon（PID 1，SIGTERM 直通）。
# 默认端口：对外 Web 8080 / daemon 8081，均可通过 env 覆盖且两者同源联动（见 start.sh 头注释）。
COPY deploy/docker/start.sh /usr/local/bin/start.sh
RUN set -eux; \
    # 日志 / 缓存目录占位（运行期由 compose tmpfs 覆盖，镜像内须先存在且属主为 65532）
    mkdir -p /var/log/nginx /var/lib/nginx/cache /var/cache/nginx /run; \
    chown -R 65532:65532 /var/log/nginx /var/lib/nginx /var/cache/nginx /run; \
    chmod 0755 /usr/local/bin/start.sh


# ---- 镜像内绝不含（红线自证清单）----
#   ✗ 私钥 / HKDF 盐 / KMS 凭据
#   ✗ 激活码 / Lease Token
#   ✗ 宿主指纹文件
#   ✗ 默认口令
#   ✗ 客户配置

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/start.sh"]
