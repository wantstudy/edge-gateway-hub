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
#   ★ 本文件不是「可直接构建物」：下方两处 FROM 的 digest 段为**占位 token**
#     （__BUILDER_DIGEST__ / __RUNTIME_DIGEST__），保留 `@sha256:` 形式。
#     构建前必须先用 render 脚本渲染出带真实 digest 的 Dockerfile：
#       deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
#       docker buildx build -f deploy/docker/Dockerfile.rendered ...
#     直接 `docker build -f deploy/docker/Dockerfile` 会因占位非法而失败
#     （fail-closed —— 让「未 pin 的构建」不可能静默成功）。
#   2) runtime 阶段只拷二进制（daemon + healthprobe）+ Web 静态资源 + 默认配置模板，
#      绝不 COPY 源码、.git、密钥、激活码、宿主指纹（persistence-layout §2）。
#   3) 以非 root 用户运行；容器内进程不申请任何 capability。
#   4) 目标架构 amd64 / arm64（buildx --platform linux/amd64,linux/arm64）。
#   5) fail-closed：产物路径错位必须让构建失败，绝不允许 BuildKit glob 静默产出空层
#      （D-09 教训：glob 无匹配 → 4.1KB 空层 → 容器 exec 失败）。
#      因此 builder 阶段对最终二进制做 `test -f` 断言，并以**固定路径** /build/out/
#      移交产物（多架构下 COPY 无条件分支，glob 是唯一坏选择，固定路径 + 断言是唯一好选择）。
#
# 构建示例（CI，勿在本机跑，见任务说明盘空间紧张）：
#   先渲染基础镜像 digest（本文件含占位 token，不可直接构建）：
#     deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -f deploy/docker/Dockerfile.rendered -t iot-daq-gateway:v1.0.0 --push .
# 离线单架构导出（build-offline-bundle.sh 已内置渲染步骤）：
#   docker buildx build --platform linux/arm64 -f deploy/docker/Dockerfile.rendered \
#     -o type=docker,dest=deploy/out/iot-daq-gateway_v1.0.0_arm64.tar .
# =============================================================================

# -----------------------------------------------------------------------------
# 阶段 1：builder —— 编译 daemon（Rust workspace 唯一运行体 crate）
# -----------------------------------------------------------------------------
# 说明：digest 段为占位 token，由 deploy/scripts/render-dockerfile-digests.sh
# 从 deploy/base-images.lock.yaml 渲染（保留 @sha256: 形式，禁可变 tag）。
# 选择 -slim 变体：体积更小、攻击面更小。
# 工具链版本：Cargo.lock 传递依赖要求 rustc ≥ 1.88（icu_properties_data 2.3.0、
# time 0.3.55 等，共 34 条 requires rustc 报错，D-04），故锁定 1.88-slim-bookworm，
# 升级走 base-images.lock.yaml 受审变更流程。
FROM rust:1.88-slim-bookworm@sha256:__BUILDER_DIGEST__ AS builder

# TARGETARCH 由 buildx 注入（amd64 / arm64），用于：
#   - Rust target triple 校验
#   - 产物命名（离线包按架构分发）
ARG TARGETARCH

# musl 静态链接（D-09b，方案 A）：distroless/static 无 loader，要求二进制完全静态。
# musl-tools 提供 musl-gcc，供 ring 等 C 依赖为 musl 目标编译 C 代码。
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends musl-tools; \
    rm -rf /var/lib/apt/lists/*

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
# 占位 lib.rs 让 cargo 能解析 workspace 成员。
# ★ 注意（D-10 教训）：绝不在预热层创建 src/main.rs —— 真实仓库的 daemon 入口是
#   src/bin/iot-daq-daemon.rs（bin 目标），COPY crates/ 不会删除残留的 dummy main.rs，
#   且 COPY 之后再用 --bin daemon 会编出空壳。dummy 只允许用 lib.rs（真实源码层会覆盖它）。
RUN set -eux; \
    for d in protocol-proto daemon licensing-server; do \
        mkdir -p "crates/$d/src"; \
        echo '' > "crates/$d/src/lib.rs"; \
    done

# 预热依赖（失败可容忍：部分 crate 需要真实源码）。不带 --bin：只编 lib 以拉取依赖。
RUN cargo build --release --package daemon || true

# ---- 真实源码层 ----
# 只拷 crates/ 下的源码；前端资源由单独 COPY 引入（dist 产物，非源码树）
COPY crates/ ./crates/

# 说明：web-console 的构建产物（dist/）由 CI 在调用 buildx 前生成，
# 本 Dockerfile 要求构建上下文 deploy/docker/context/web-console-dist/ 已存在
# （由 deploy/scripts/build-offline-bundle.sh 预置）。
# ★ COPY 没有「缺省时跳过」语义：目录缺失 = 构建失败（fail-closed，D-09 同源教训）。
COPY deploy/docker/context/web-console-dist/ /build/web-console-dist/

# 健康探针源码（纯 std 零依赖，见 deploy/docker/healthprobe/main.rs 头注释，D-06）
COPY deploy/docker/healthprobe/main.rs /build/healthprobe.rs

# 编译：release profile 已含 strip + LTO + panic=abort（见根 Cargo.toml）
# musl 静态（D-09b 方案 A）：--target <musl triple> + crt-static；
# 产物路径 <triple>/release/iot-daq-daemon 必须经 test -f 断言 + 拷贝到固定路径
# /build/out/（D-09：glob 错位在多架构 COPY 下无法表达，固定路径是唯一安全做法）。
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
# 阶段 2：runtime —— distroless 静态镜像，非 root，只读友好
# -----------------------------------------------------------------------------
# gcr.io/distroless/static-debian12：无 shell、无包管理器、无 libc 动态依赖，
# 攻击面最小；二进制为 musl 静态链接（D-09b 方案 A，builder 阶段 crt-static 编出，
# 与本基镜像匹配；若未来改用 glibc 动态二进制，必须换
# gcr.io/distroless/cc-debian12 同款 digest pin 方案并同步本注释与 deploy/README.md）。
FROM gcr.io/distroless/static-debian12:nonroot@sha256:__RUNTIME_DIGEST__ AS runtime

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

# ---- 仅拷贝运行时产物：daemon 二进制（builder 阶段已 test -f 断言，D-09）----
COPY --from=builder --chown=65532:65532 /build/out/iot-daq-daemon /usr/local/bin/iot-daq-daemon

# ---- 仅拷贝运行时产物：healthprobe 静态探针（纯 std 零依赖，D-06）----
# 供 compose healthcheck exec-form 调用：distroless 无 shell，无法跑 CMD-SHELL。
COPY --from=builder --chown=65532:65532 /build/out/healthprobe /usr/local/bin/healthprobe

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
