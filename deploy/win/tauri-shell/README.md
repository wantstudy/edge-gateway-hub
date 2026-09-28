# tauri-shell — Windows 桌面壳（Tauri 2.x）

IoT-DAQ 工业边缘网关的 **Windows 桌面壳**：用 Tauri 2.x 把 `web-console`（网关侧 Vue 3 管理界面）承载进 WebView 窗口，并按需拉起 `daemon` 侧车（核心守护进程）。产物为 **NSIS 安装包（`.exe`）**，在 CI（`windows-latest`）上自动构建，可选 Authenticode 签名。

> 设计契约（详见根 `README.md` 与 `.omo/plans/iot-daq-gateway.md`）：
> - **授权判定始终在 Rust 侧（daemon）**，WebView / JS 层只做展示，无任何授权逻辑。
> - 防逆向 Tier-1：release 产物 `strip + LTO + panic=abort`（已在 `src-tauri/Cargo.toml` 落实）；代码签名经 CI 的 Authenticode 步骤完成。
> - 本工程为**独立 Cargo 工程**（顶部 `[workspace]` 脱离根 workspace），避免 Windows-only 依赖进入根 workspace、影响 Linux/macOS 的 `cargo build --workspace`。

## 目录结构

```
tauri-shell/
├── package.json              # 前端脚本（tauri-cli 通过 @tauri-apps/cli 提供）
├── .gitignore
└── src-tauri/
    ├── Cargo.toml            # 独立 Cargo 工程
    ├── build.rs
    ├── tauri.conf.json       # 窗口 / 打包（NSIS）/ 资源 / CSP
    ├── capabilities/         # 权限（仅 core:default）
    ├── src/main.rs           # 入口：建窗口 + 拉起 daemon 侧车
    ├── icons/                # 应用图标（含 source-icon.png，CI 用其重生成全平台图标）
    └── resources/            # （可选）放置 daemon 侧车二进制，由壳运行时拉起
```

## 本地构建前提

- Windows 10/11（仅 Windows 目标）
- [Rust 稳定版](https://rustup.rs/) + target `x86_64-pc-windows-msvc`
- [Node 22](https://nodejs.org/) + npm
- [NSIS](https://nsis.sourceforge.io/)（Tauri NSIS 打包需要 `makensis` 在 PATH）
- Visual Studio Build Tools（MSVC `cl.exe`，用于 rusqlite bundled 等 C 编译；daemon 侧车启用时需要）
- WebView2 Runtime（Windows 11 自带；Win10 需安装）

> ⚠️ 本机（开发沙箱）未预装 `tauri-cli` / `makensis` / 代码签名证书，因此**无法在此亲手打出签名的 `.exe`**。本工程以「CI 就绪」方式交付：推送 `v*` 标签或手动 `workflow_dispatch` 即在 `windows-latest` 上自动出包（见 `.github/workflows/release-windows.yml`）。

## 本地开发 / 构建

```bash
# 1) 安装依赖（首次）
npm --prefix ../../../frontends/web-console ci
npm --prefix . ci

# 2) 调试运行（热重载 web-console）
npm run dev

# 3) 产出安装包（NSIS .exe）
npm run build
# 产物：src-tauri/target/release/bundle/nsis/IoT-DAQ Gateway_0.1.0_x64-setup.exe
```

> 若本地已 `npm --prefix ../../../frontends/web-console run build` 过，`npm run build`（tauri build）的
> `beforeBuildCommand` 也会自动构建 `web-console`，无需手动预构建。

## daemon 侧车（核心进程）

设计上 daemon 以**侧车**形式随包分发，由壳在启动时从 `resource_dir()` 拉起，UI 经 `127.0.0.1:8080` 与其通信。

- **当前状态**：**已启用**。`crates/daemon` 提供 `[[bin]] iot-daq-daemon`（task 33，thin main：
  预加载配置 → 管理面 REST/SSE/静态服务 → `bootstrap::run` 全权接管），CI 每次打包都会
  构建并随 NSIS 安装包分发（`Build daemon sidecar` 步骤）。
- **降级行为**：`src/main.rs` 在 `resource_dir()` 下找不到 `iot-daq-daemon.exe` 时，仅打印
  警告并以「纯 UI 模式」运行——`web-console` 走其内置 mock / 受限后端（授权判定本就不在
  UI 层，故不影响安全红线）。
- **本地构建注意**：`tauri.conf.json` 已声明 `resources/iot-daq-daemon.exe` 映射，本地
  `npm run build` 前**必须先产出侧车**，否则打包缺资源失败：
  ```bash
  cargo build --release -p daemon --bin iot-daq-daemon
  mkdir -p src-tauri/resources
  cp ../../../target/release/iot-daq-daemon.exe src-tauri/resources/
  ```
- **daemon 运行参数**（`resource_dir()` 拉起时走默认值；需要定制时经壳内环境变量注入）：
  `IOT_DAQ_CONFIG`（默认 `./config.toml`）、`IOT_DAQ_MGMT_BIND`（默认 `127.0.0.1:8080`）、
  `IOT_DAQ_WEB_DIST`（默认 `./web-dist`）。

## 安装包签名（Authenticode）

CI 通过 `workflow_dispatch` 勾选 `sign=true` 并在仓库 **Secrets** 中配置：

- `WINDOWS_CERTIFICATE`：PFX 证书的 **base64** 字符串（`base64 -w0 cert.pfx`）
- `WINDOWS_CERTIFICATE_PASSWORD`：PFX 密码

CI 会把证书导入当前用户证书存储，并用 `Set-AuthenticodeSignature`（SHA256 + DigiCert 时间戳）对安装包签名；缺失证书时自动跳过（产出未签名包）。

> 进阶：也可改用 Tauri 原生 `bundle.windows.nsis.signCommand` 在打包阶段签名（含对内部主程序先签再封包）。当前实现先封包再签安装包，已满足「安装包可被验证、篡改失败」的契约；如需对内部 `iot-daq-gateway.exe` 单独签名，可在 `tauri build` 前增加一步 `signtool sign`。

## CI 流水线

见 [`.github/workflows/release-windows.yml`](../../../.github/workflows/release-windows.yml)：

1. Checkout → 装 Rust（MSVC target）+ Node 22
2. `npm ci` 安装 web-console 与 tauri-cli 依赖
3. `tauri icon` 用源 PNG 重生成全平台图标
4. （可选）构建 daemon 侧车
5. `tauri build` → NSIS 安装包
6. （可选）Authenticode 签名
7. 上传 artifact `iot-daq-gateway-windows-installer`
