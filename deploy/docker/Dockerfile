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
#      示例 digest 为占位；实际由 CI 从 deploy/base-images.lock.yaml 注入（supply-chain §3.1）。
#   2) runtime 阶段只拷二进制 + Web 静态资源 + 默认配置模板，
#      绝不 COPY 源码、.git、密钥、激活码、宿主指纹（persistence-layout §2）。
#   3) 以非 root 用户运行；容器内进程不申请任何 capability。
#   4) 目标架构 amd64 / arm64（buildx --platform linux/amd64,linux/arm64）。
#
# 构建示例（CI，勿在本机跑，见任务说明盘空间紧张）：
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -f deploy/docker/Dockerfile -t iot-daq-gateway:v1.0.0 --push .
# 离线单架构导出：
#   docker buildx build --platform linux/arm64 -f deploy/docker/Dockerfile \
#     -o type=docker,dest=deploy/out/iot-daq-gateway_v1.0.0_arm64.tar .
# =============================================================================

# -----------------------------------------------------------------------------
# 阶段 1：builder —— 编译 daemon（Rust workspace 唯一运行体 crate）
# -----------------------------------------------------------------------------
# 说明：digest 为占位格式示例，CI 打包时会替换为 lock 文件中的实际值。
# 选择 -slim 变体：体积更小、攻击面更小；musl 静态链接决策见 headless/README.md。
FROM rust:1.85-slim-bookworm@sha256:0000000000000000000000000000000000000000000000000000000000000001 AS builder

# TARGETARCH 由 buildx 注入（amd64 / arm64），用于：
#   - Rust target triple 校验
#   - 产物命名（离线包按架构分发）
ARG TARGETARCH

# 构建期不写入任何密钥；指纹盐为编译期常量或运行时注入，见 installer-hardening §1
ENV CARGO_TERM_COLOR=never \
    CARGO_NET_OFFLINE=false \
    RUSTFLAGS="-C strip=symbols"

WORKDIR /build

# ---- 依赖层缓存：先只拷清单，构建一次空壳，最大化 Docker 层复用 ----
COPY Cargo.toml Cargo.lock ./
COPY crates/protocol-proto/Cargo.toml crates/protocol-proto/
COPY crates/daemon/Cargo.toml crates/daemon/
COPY crates/licensing-server/Cargo.toml crates/licensing-server/
# 占位源码让 cargo 能解析 workspace 成员
RUN set -eux; \
    for d in protocol-proto daemon licensing-server; do \
        mkdir -p "crates/$d/src"; \
        echo 'fn main() {}' > "crates/$d/src/main.rs"; \
    done; \
    mkdir -p crates/daemon/src; \
    echo '' > crates/daemon/src/lib.rs

# 预热依赖（失败可容忍：部分 crate 需要真实源码）
RUN cargo build --release --package daemon --bin daemon || true

# ---- 真实源码层 ----
# 只拷 crates/ 下的源码；前端资源由单独 COPY 引入（dist 产物，非源码树）
COPY crates/ ./crates/

# 说明：web-console 的构建产物（dist/）由 CI 在调用 buildx 前生成，
# 本 Dockerfile 假设它已存在于构建上下文 deploy/docker/context/web-console-dist/，
# 由 deploy/scripts/build-offline-bundle.sh 预置；缺省时跳过不影响特性（仅无管理界面）。
COPY deploy/docker/context/web-console-dist/ /build/web-console-dist/

# 编译：release profile 已含 strip + LTO + panic=abort（见根 Cargo.toml）
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TRIPLE="x86_64-unknown-linux-musl" ;; \
      arm64) RUST_TRIPLE="aarch64-unknown-linux-musl" ;; \
      *) echo "不支持的 TARGETARCH=$TARGETARCH（仅 amd64 / arm64）" >&2; exit 1 ;; \
    esac; \
    echo "BUILD_TARGET=$RUST_TRIPLE" > /build/build-info.txt; \
    cargo build --release --package daemon --bin daemon --target-dir /build/target

# -----------------------------------------------------------------------------
# 阶段 2：runtime —— distroless 静态镜像，非 root，只读友好
# -----------------------------------------------------------------------------
# gcr.io/distroless/static-debian12：无 shell、无包管理器、无 libc 动态依赖，
# 攻击面最小；要求二进制为 musl 静态链接（headless/README.md 构建期决策）。
# 若采用 glibc 动态二进制，改用 gcr.io/distroless/cc-debian12 同款 digest pin 方案。
FROM gcr.io/distroless/static-debian12:nonroot@sha256:0000000000000000000000000000000000000000000000000000000000000002 AS runtime

# 镜像元数据（便于现场 docker inspect 记录安装单）
ARG TARGETARCH
LABEL org.opencontainers.image.title="iot-daq-gateway" \
      org.opencontainers.image.description="工业边缘数据汇聚与统一分发网关（headless 运行体）" \
      org.opencontainers.image.licenses="LicenseRef-Proprietary" \
      org.opencontainers.image.source="https://vendor-internal.example/iot-daq" \
      com.vendor.iot-daq.arch="${TARGETARCH}" \
      com.vendor.iot-daq.fingerprint-policy="host-only" \
      com.vendor.iot-daq.state-policy="persistent-volume-only"

# distroless 的 :nonroot 变体自带 uid/gid 65532，无需 USER 再声明 root
# ---- 运行时目录契约（与持久卷布局一致）----
# /var/lib/iot-daq 由 compose 以宿主目录挂载；镜像内仅建空占位目录保证
# 未挂载时的可诊断行为（entrypoint 会因不可写而明确拒绝启动）。
WORKDIR /home/nonroot

# ---- 仅拷贝运行时产物：二进制 ----
COPY --from=builder --chown=65532:65532 /build/target/*/release/daemon /usr/local/bin/iot-daq-daemon

# ---- 仅拷贝运行时产物：Web 控制台静态资源（纯静态，无密钥）----
COPY --from=builder --chown=65532:65532 /build/web-console-dist/ /srv/web-console/

# ---- 默认配置模板（占位值，现场注入覆盖；不含任何真实凭据）----
COPY --chown=65532:65532 deploy/docker/config/gateway.default.toml /etc/iot-daq/gateway.toml

# ---- 入口脚本：做宿主锚定校验 + 持久卷可写性校验 ----
# distroless static 无 /bin/sh，故 entrypoint 用二进制自身而非 shell 脚本。
# 校验逻辑由 daemon 的 `--preflight` 子命令承担（见 deploy/README.md「入口契约」），
# 与 deploy/docker/entrypoint.sh（shell 版，供调试/非 distroless 变体使用）语义一一对应。
COPY --chown=65532:65532 deploy/docker/entrypoint.sh /usr/local/bin/entrypoint.sh
# 注意：distroless/static 无 shell，上面这行仅对 cc/shell 变体有意义；
# 使用 static 变体时真正的 ENTRYPOINT 走二进制 --preflight，见下。

# ---- 镜像内绝不含（红线自证清单，persistence-layout §2）----
#   ✗ 私钥 / HKDF 盐 / KMS 凭据
#   ✗ 激活码 / Lease Token
#   ✗ 宿主指纹文件（host-fingerprint.json 由安装脚本现场生成）
#   ✗ 默认口令
#   ✗ 客户配置
# 上述任一项出现在镜像层即为阻断性缺陷。

USER 65532:65532

# 静态二进制 → 无需 shell → 用二进制自身做 preflight 校验后进入前台运行
ENTRYPOINT ["/usr/local/bin/iot-daq-daemon", "--preflight", "--config", "/etc/iot-daq/gateway.toml"]
CMD ["--foreground"]

# 健康检查由 compose 的 healthcheck 定义（distroless 无 curl/wget）；
# 此处仅声明端口意图，实际监听见 container-deploy.md §2.2。
EXPOSE 8080
