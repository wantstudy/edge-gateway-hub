# iot-daq — 工业边缘数据汇聚与统一分发网关

跨平台（Windows Tauri 桌面 + Linux headless/Docker）工业边缘网关：南向采集 Modbus / OPC UA / S7 / MC / HTTP / 第三方 MQTT，北向经 MQTT 统一分发（protobuf 默认 / json 可选）。授权体系：云端 Lease Token（Ed25519）+ 消息级 AuthBlock 二次校验 + 7 天离线宽限。

> 设计基线见 `docs/design/`（授权与防破解、一机一码、激活码生命周期、容器化、两端界面）；任务计划见 `.omo/plans/iot-daq-gateway.md`。

## 仓库结构

| 目录 | 说明 | 状态 |
|---|---|---|
| `crates/daemon` | 核心守护进程库（驱动 + 处理 + 缓存 + MQTT + 授权 + 管理 API） | Wave 1 骨架 |
| `crates/licensing-server` | 云授权服务（激活 / 心跳 / Token 签发 / 激活码生命周期） | Wave 1 骨架 |
| `crates/protocol-proto` | 北向 Protobuf schema（TelemetryBatch / DataPoint / AuthBlock） | Wave 1 骨架 |
| `tauri-shell/` | Windows Tauri 2.x 桌面壳（承载 web-console + 拉起 daemon 侧车，产出 NSIS 签名安装包 `.exe`） | **工程已搭（独立 Cargo 工程，CI 就绪）；daemon 侧车待 `crates/daemon` 增二进制入口后启用** |
| `headless/` | Linux headless 服务（AppImage/deb/rpm + systemd，容器运行体） | 占位（按计划 Wave 4 task 33，尚未开工） |
| `web-console/` | 网关侧 Vue 3 管理界面（ui-kit + Arco Design Vue） | **已实现（16 页，CDP 真机验证 23/23）** |
| `admin-console/` | 厂商总管理后台（激活码 / 设备 / 租约 / 租户管理） | **已实现（Vue 3 + ui-kit，12 页）** |
| `ui-kit/` | 两端共用前端基础包（设计 token + 共享业务组件 + RBAC + 状态映射） | **已实现** |
| `deploy/` | 容器与部署资产（Dockerfile / compose / entrypoint / install / uninstall / 离线打包 / cosign 验签，非 Cargo 成员） | **已实现** |
| `docs/design/` | Wave 0 定稿设计图与界面设计 | 已入库 |

## 构建与测试

```bash
cargo build --workspace   # 构建全部 Rust 成员
cargo test --workspace    # CI 门禁（task 7 起）
```

## 前端（web-console / admin-console）

两端前端均为 Vite + Vue 3 + TypeScript，共用 `@ui-kit` 基础包（设计 token + 共享业务组件 + RBAC）。本机构建前**务必先清掉残留 HTTP 代理**，否则 npm/vite 连接复用会被劫持导致「首请求成功、后续失败」：

```bash
# 任一前端目录下
export HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy= NO_PROXY='*'
npm install
npm run build          # 生产构建（类型检查 vue-tsc + vite build 一体）
npm run preview        # 本地预览（hash 路由，无需服务端 rewrite）
```

- `web-console`：网关侧控制台，16 个页面（总览 / 实时监控 / 告警 / 设备接入 / 新增设备 / 点位与映射 / 北向转发 / 转发规则 / 日志审计 / 诊断 / 备份 / 系统更新 / 启动与自启 / 授权激活 / 账号角色 / 系统设置）。角色门控（`RoleGate`）仅控制可见性，授权判定一律在 Rust 侧。
- `admin-console`：厂商侧后台，12 个页面，角色模型来自 `ui-kit/src/rbac.ts`（ops / lic_ops / risk / system）。

## 部署（Linux Docker，主推形态）

`deploy/` 提供完整可离线部署的交付包（详见 `deploy/README.md`）。要点：

- **多架构镜像**：`docker buildx build --platform linux/amd64,linux/arm64`，基础镜像与产物均 **digest pin**，禁用可变 tag / `:latest`。
- **宿主指纹锚点（陷阱 1）**：容器内机器码只取自宿主机（只读挂载 `/etc/machine-id`、`/sys/class/dmi/id`，宿主 MAC 由 `install.sh` 采集并 HMAC 签名写入 `host-fingerprint.json`）；`IOT_DAQ_ALLOW_CONTAINER_ANCHORS=0` 禁止回退到容器内标识。
- **授权持久化（陷阱 2）**：`read_only: true` + 单一 rw 卷 `${IOT_DAQ_DATA_DIR}:/var/lib/iot-daq`，试用 / 租约 / 授权状态必须落宿主持久卷，`docker rm && docker run` 不会重置。
- **安全红线**：非 root（UID 65532）、`cap_drop: ALL`、`no-new-privileges`；**禁 `--privileged`、禁挂 `/var/run/docker.sock`**、镜像层不含任何密钥 / 激活码 / 宿主指纹。
- **离线交付**：`deploy/scripts/build-offline-bundle.sh` 产出镜像 tar + `SHA256SUMS`；`deploy/scripts/sign-and-verify.sh` 做 `cosign verify` 与离线包哈希校验；`deploy/scripts/install.sh` 一键（校验 → load → 注入宿主指纹 → `docker compose up -d` → 健康检查）。

> **windows-gnu 工具链注意**：本机使用 `stable-x86_64-pc-windows-gnu`，构建命令统一使用前缀
> `PATH="/d/rust/cargo/bin:/d/rust/mingw64/bin:$PATH"`（winlibs mingw-w64 独立发行版：
> gcc 16.2 + binutils 2.47，提供 rustc raw-dylib 链接所需的 dlltool + as；后续 Wave 2b 的
> C 依赖如 rusqlite bundled 也依赖 mingw gcc）。**顺序红线**：mingw64/bin 必须在 rustup
> self-contained 目录之前（或直接省略 self-contained）——否则 self-contained 的 ld 与
> mingw gcc 不匹配，报 `cannot find -lmsvcrt`。新增依赖时仍需确认
> 纯 Rust 约束与 `cargo deny`（Wave 7 起）。

## Windows 桌面壳（tauri-shell）

`tauri-shell/` 是独立 Cargo 工程（脱离根 workspace，保持 `cargo build --workspace` 在 Linux/macOS CI 绿灯），承载 `web-console` 进 WebView 并按需拉起 `daemon` 侧车，产出 **NSIS 安装包 `.exe`**。

- **CI 自动出包**：`.github/workflows/release-windows.yml` 在 `windows-latest` 上装 Rust（MSVC target）+ Node 22 → 构建 web-console → `tauri build` 产出 NSIS 包；勾选 `sign` 并经仓库 Secrets（`WINDOWS_CERTIFICATE` / `WINDOWS_CERTIFICATE_PASSWORD`）做 Authenticode 签名（缺证书自动跳过）。
- **授权判定始终在 Rust 侧**：WebView/JS 只展示，壳经 `127.0.0.1:8080` 与 daemon 通信；缺失 daemon 二进制时以「纯 UI 模式」降级（不影响安全红线）。
- **防逆向 Tier-1**：release 产物 `strip + LTO + panic=abort`（已在 `src-tauri/Cargo.toml` 落实），代码签名经 CI 完成。
- **本地构建前提**：Windows + Rust(MSVC target) + Node 22 + NSIS(`makensis`) + WebView2；详见 `tauri-shell/README.md`。开发沙箱未预装 `tauri-cli`/`makensis`/签名证书，故本机不出包，以「CI 就绪」方式交付。
- **daemon 侧车**：当前 `crates/daemon` 仍是库 crate（尚无二进制入口 / 管理 API 服务端），默认不打包；待其增加 `[[bin]]` 后按 `tauri-shell/README.md` 的「启用侧车」步骤接入。

## 约束

- **平台矩阵**：CI 仅 `windows-latest` / `ubuntu-latest` 双平台，不含 macOS。
- **纯 Rust 依赖栈**：禁止引入需系统 C 库 / cmake / NASM 的依赖（`openssl-sys` 非 vendored、`native-tls`、`paho-mqtt`、`aws-lc-rs` 等）。**vendored C 源允许**——`rusqlite` 的 `bundled` 特性（内嵌 SQLite C 源）已实测在 mingw 下构建 `EXIT=0`，是合规的 SQLite 接入方式；真正禁的是「要求系统已装 C 编译器/库的裸链接」依赖。
- **安全红线**：仓库内不提交任何激活码 / 私钥 / 真实机器码；release 产物按 Tier-1 基线 strip + LTO + panic=abort。
- **JSON 编码约定**：纳秒时间戳 / uint64 计数器在 JSON 路径必须字符串编码（`int64` → string），见 `crates/protocol-proto`。
