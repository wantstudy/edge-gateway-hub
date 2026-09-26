# =============================================================================
# iot-daq 授权服务（licensing-server）容器镜像 · 多阶段构建
# =============================================================================
# 定位：与 deploy/docker/Dockerfile（网关 daemon 专用）**并列的独立交付物**，
#   两者互不影响（本文件不改动、不覆盖网关那套）。
#
# 设计依据：
#   deploy/docker/Dockerfile                  同款多阶段 / digest pin / 非 root 范式
#   deploy/base-images.lock.yaml              基础镜像 digest 唯一事实源
#   deploy/scripts/render-dockerfile-digests.sh  渲染入口
#
# 硬约束（违反即视为缺陷）：
#   1) 基础镜像一律 `FROM <image>:<tag>@sha256:<digest>`（禁 `:latest` / 可变 tag）。
#      digest 不手写：本文件含 __BUILDER_DIGEST__ / __RUNTIME_DIGEST__ 占位 token，
#      构建前必须渲染：
#        deploy/scripts/render-dockerfile-digests.sh \
#          --template deploy/docker/licensing-server.Dockerfile \
#          --output   deploy/docker/licensing-server.Dockerfile.rendered
#      ★ 直接 `docker build -f 本文件` 会因占位非法而失败（fail-closed）。
#   2) 只在容器内构建产物，服务器上绝不构建。
#   3) runtime 只拷二进制（licensing-server + healthprobe），不拷源码 / 密钥 / 库文件。
#   4) 非 root（distroless :nonroot = uid/gid 65532）运行。
#   5) fail-closed：产物路径错位必须构建失败（test -f 断言），杜绝空层。
#
# 运行期契约（见 crates/licensing-server/src/main.rs 头注释）：
#   IOT_DAQ_LISTEN_ADDR          默认 0.0.0.0:7080
#   IOT_DAQ_LICENSE_DB           默认 ./licensing.db（生产必须显式指到挂载卷）
#   IOT_DAQ_LOG_LEVEL            默认 info
#   IOTDAQ_ADMIN_USER / IOTDAQ_ADMIN_PASSWORD_SHA256 | IOTDAQ_ADMIN_PASSWORD
#   IOTDAQ_JWT_SECRET
#   IOT_DAQ_LICENSE_SIGNING_KID / IOT_DAQ_LICENSE_SIGNING_KEY（须成对）
#   —— 全部经环境变量注入（本服务不读 config.toml，那是 daemon 的）。
#
# musl 静态链接：runtime 用 gcr.io/distroless/static-debian12（无 loader），
#   要求二进制完全静态；rusqlite `bundled` 需要 C 编译器，由 musl-tools 的
#   musl-gcc 在 builder 内完成，故**必须在 Linux 容器内构建**（Windows 宿主不可交叉编译）。
# =============================================================================

# -----------------------------------------------------------------------------
# 阶段 1：builder —— 编译 licensing-server
# -----------------------------------------------------------------------------
# digest 段为占位 token，由 render-dockerfile-digests.sh 从 base-images.lock.yaml 注入。
FROM rust:1.88-slim-bookworm@sha256:__BUILDER_DIGEST__ AS builder

# TARGETARCH 由 buildx 注入（amd64 / arm64），决定 Rust target triple。
ARG TARGETARCH

# 与最终构建一致的 RUSTFLAGS，保证「依赖预热层」与「真实构建层」指纹相同、缓存可复用。
ENV CARGO_TERM_COLOR=never \
    RUSTFLAGS="-C strip=symbols -C target-feature=+crt-static"

WORKDIR /build

# musl 静态工具链 + 目标 target（rusqlite bundled 需要 C 编译器 → musl-gcc）。
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends musl-tools; \
    rm -rf /var/lib/apt/lists/*; \
    case "$TARGETARCH" in \
      amd64) echo "x86_64-unknown-linux-musl"  > /tmp/rust-triple ;; \
      arm64) echo "aarch64-unknown-linux-musl" > /tmp/rust-triple ;; \
      *) echo "不支持的 TARGETARCH=$TARGETARCH（仅 amd64 / arm64）" >&2; exit 1 ;; \
    esac; \
    rustup target add "$(cat /tmp/rust-triple)"

# ---- 依赖层缓存：先只拷清单 + 占位 lib.rs，拉取并编译依赖 ----
# ★ 占位只允许用 lib.rs：绝不在预热层造 src/main.rs（COPY 不会删除残留 dummy，
#   会让后续 --bin 编出空壳；D-10 教训）。
COPY Cargo.toml Cargo.lock ./
COPY crates/protocol-proto/Cargo.toml crates/protocol-proto/
COPY crates/daemon/Cargo.toml crates/daemon/
COPY crates/licensing-server/Cargo.toml crates/licensing-server/
RUN set -eux; \
    for d in protocol-proto daemon licensing-server; do \
      mkdir -p "crates/$d/src"; \
      echo '' > "crates/$d/src/lib.rs"; \
    done; \
    cargo build --release --package licensing-server --target "$(cat /tmp/rust-triple)" || true

# ---- 真实源码层 ----
COPY crates/ ./crates/

# 健康探针源码（纯 std 零依赖，distroless 无 shell，供 compose healthcheck exec-form 调用）
COPY deploy/docker/healthprobe/main.rs /build/healthprobe.rs

# 编译 + 产物断言 + 固定路径移交（多架构下 glob 无法表达，固定路径是唯一安全做法）
#
# ★ 关键（D-10 同类陷阱，实测踩到）：依赖预热层用**空壳 lib.rs** 编过一次本 crate，
#   而 Docker COPY 保留源码的原始 mtime（通常**早于**预热层里刚生成的 dummy），
#   cargo 的 mtime 指纹会把真实源码误判为「未变更」→ 复用空壳 rlib →
#   真实 main.rs 链接时报 `unresolved import licensing_server::<mod>`。
#   因此这里必须显式：① 触碰真实源码 mtime 到当前时刻；② 清掉本 crate 的陈旧
#   产物与指纹（只清本 crate，依赖层缓存不受影响），确保一定用真实源码重编。
RUN set -eux; \
    TRIPLE="$(cat /tmp/rust-triple)"; \
    find /build/crates/licensing-server -type f -name '*.rs' -exec touch {} +; \
    cargo clean --release --package licensing-server \
        --target "$TRIPLE" --target-dir /build/target 2>/dev/null || true; \
    cargo build --release --package licensing-server --bin licensing-server \
        --target "$TRIPLE" --target-dir /build/target; \
    BIN="/build/target/${TRIPLE}/release/licensing-server"; \
    if [ ! -f "$BIN" ]; then \
      echo "FATAL: licensing-server 二进制不存在：$BIN（产物路径错位，fail-closed 拒绝空层）" >&2; \
      exit 1; \
    fi; \
    rustc --target "$TRIPLE" -C target-feature=+crt-static -C strip=symbols \
        -O /build/healthprobe.rs -o /build/out_healthprobe; \
    test -f /build/out_healthprobe; \
    mkdir -p /build/out; \
    cp "$BIN" /build/out/licensing-server; \
    cp /build/out_healthprobe /build/out/healthprobe; \
    echo "BUILD_TARGET=$TRIPLE" > /build/out/build-info.txt

# -----------------------------------------------------------------------------
# 阶段 2：runtime —— distroless 静态镜像，非 root，只读友好
# -----------------------------------------------------------------------------
FROM gcr.io/distroless/static-debian12:nonroot@sha256:__RUNTIME_DIGEST__ AS runtime

ARG TARGETARCH
LABEL org.opencontainers.image.title="iot-daq-licensing-server" \
      org.opencontainers.image.description="IoT-DAQ 云端授权服务（激活 / 心跳 / Lease Token 签发 / 激活码生命周期）" \
      org.opencontainers.image.licenses="LicenseRef-Proprietary" \
      org.opencontainers.image.source="https://vendor-internal.example/iot-daq" \
      com.vendor.iot-daq.arch="${TARGETARCH}" \
      com.vendor.iot-daq.role="licensing-server"

WORKDIR /home/nonroot

# 仅运行时产物：licensing-server 二进制（builder 已 test -f 断言）
COPY --from=builder --chown=65532:65532 /build/out/licensing-server /usr/local/bin/licensing-server
# 仅运行时产物：healthprobe 静态探针（供 compose healthcheck 使用）
COPY --from=builder --chown=65532:65532 /build/out/healthprobe /usr/local/bin/healthprobe

# ---- 镜像内绝不含（红线自证）----
#   ✗ 管理员明文口令 / 口令摘要 / JWT 密钥 / Lease 签名私钥（一律运行期环境变量注入）
#   ✗ 客户数据 / licensing.db（挂载卷持久化）
#   ✗ 源码 / .git

# distroless :nonroot 自带 uid/gid 65532；显式声明避免误以 root 运行
USER 65532:65532

EXPOSE 7080

# 无 shell → 直接 exec 二进制；监听地址 / 库路径由环境变量注入
ENTRYPOINT ["/usr/local/bin/licensing-server"]
