# iot-daq — 工业边缘数据汇聚与统一分发网关

跨平台工业边缘网关：**南向**采集 Modbus TCP/RTU、OPC UA、S7、MC（三菱）、HTTP、第三方 MQTT；
**北向**经 MQTT 统一分发（protobuf 默认 / json 可选）。授权体系为
**一机一码 + 云端 Lease Token（Ed25519）+ 消息级 AuthBlock 二次校验 + 离线宽限**。

交付形态两端：

- **Windows**：`tauri-shell/` 桌面壳（Tauri 2.x 承载 `web-console` + 拉起 daemon 侧车）→ NSIS 安装包 `.exe`
- **Linux**：Docker 容器（容器内置 nginx 提供静态页面 + `/api/*` 反代到 daemon，非 root / 只读根文件系统 / 宿主锚定）

设计基线见 `docs/design/`，任务计划见 `.omo/plans/iot-daq-gateway.md`
（两者均**不在版本库内**，见文末「已知边界」）。

---

## 一、项目简介

### 1.1 系统组成

| 组件 | 语言/形态 | 职责 |
|---|---|---|
| **`crates/daemon`（网关端守护进程）** | Rust 库 + `[[bin]] iot-daq-daemon` | 采集调度与热重建、六个南向驱动、公式计算与点位模拟、转发规则引擎（结构化 `when`/`actions`）、断网续传队列、遥测/审计落盘（SQLite）、告警引擎、北向 MQTT 池化分发（protobuf/json + AuthBlock 签名）、管理面 REST + SSE + 静态页面服务、授权运行期装配、计划重启 |
| **`crates/licensing-server`（授权端）** | Rust + axum | 管理员登录（JWT）、激活码生命周期（生成/发放/废弃/重发）、租户与设备登记、Lease Token 签发（Ed25519）、心跳与激活响应签名、换机工单、回执与审计 |
| **`crates/protocol-proto`（协议层）** | Rust + prost | 北向 Protobuf schema（`TelemetryBatch` / `DataPoint` / `AuthBlock`）；JSON 路径下 int64 → string 约定 |
| **`web-console`（网关控制台）** | Vue 3 + Vite + TS | 现场工程师视角：设备接入 / 点位与映射 / 北向转发 / 实时监控 / 告警 / 日志审计 / 诊断 / 备份 / 系统更新 / 启动与自启 / 授权激活 / 账号角色 / 系统设置（18 个页面组件） |
| **`admin-console`（厂商后台）** | Vue 3 + Vite + TS | 厂商视角：激活码 / 设备 / 租户 / 密钥 / 审计 / 换机工单（12 个页面组件） |
| **`ui-kit`（共用前端包）** | 纯源码包 | 设计 token（`tokens.ts` ⇄ `tokens.css`）、明暗双主题、RBAC 矩阵、状态映射、掩码/时间纯函数、共享业务组件与表单原语 |
| **`tauri-shell`（Windows 桌面壳）** | 独立 Cargo 工程 + `@tauri-apps/cli` | 承载 `web-console` 进 WebView、探测空闲端口并拉起 daemon 侧车、产出 NSIS 签名安装包 |
| **`deploy`（部署资产）** | Dockerfile / compose / shell | 多架构镜像（digest pin）、离线包、安装/卸载、验签、宿主指纹采集、授权端单机 compose、宝塔宿主 nginx vhost |

### 1.2 核心语义

**一机一码（机器绑定）**
机器码锚点 **≥3 个且全部取自宿主机**：宿主 `/etc/machine-id`、`/sys/class/dmi/id` 的
`product_uuid` / `board_serial`、安装脚本采集的首个物理网卡 MAC（HMAC 签名写入
`host-fingerprint.json`）。容器内 `/etc/machine-id`、veth MAC、容器主机名**一律禁用**为锚点
——容器重建即变，会导致授权失效。参照 `docs/design/container-machine-binding.md`。

**授权与校验**
云端 `licensing-server` 用 Ed25519 签发 Lease Token；网关侧在**北向消息**上附
`AuthBlock` 做二次校验（消息级而非仅启动级）。激活响应携带 `server_pubkey` 并被客户端
**钉定**（TOFU），后续心跳响应用钉定公钥验签——缺签 / 篡改 / 异钥一律 fail-closed。

**试用与降级**
`trial_days` 默认 **3** 天；试用到期、离线宽限归零、授权标记损坏或被篡改 → 进入
`Degraded` 状态。**降级 ≠ 停用**：`Degraded` 只关闭北向转发，**本地采集与入队照常继续**
（并可回落到免费版配额：设备数 / 采集间隔门控）。任何状态恢复（心跳或激活成功）即回到
`Licensed`。实现见 `crates/daemon/src/license.rs`。

**离线宽限**
`grace_days` 默认 **7** 天，起点为「最后一次成功联网时刻」；宽限内保持 `Licensed`，
归零才降级。审计 / 授权日志**不走 stdout**，落持久卷由应用自身滚动。

---

## 二、目录说明

```
F:/xy/iot-daq
├── Cargo.toml                  Rust workspace（3 成员）；release 基线 strip+LTO+panic=abort
├── config.example.toml         网关配置模板（全字段注释；本地联调起点）
├── deny.toml                   cargo-deny 供应链合规门禁配置
├── .github/workflows/          CI：build.yml（构建/测试/镜像门禁）/ deny.yml / release-windows.yml
│
├── crates/
│   ├── daemon/                 网关核心守护进程
│   │   ├── src/auth/           授权客户端：指纹、组装、时钟、密钥托管、试用、限额、收据上报
│   │   ├── src/driver/         南向驱动：modbus / opcua / s7(+adapter) / mc / http / mqtt_in
│   │   ├── src/mgmt/           管理面：RBAC、登录与 bootstrap、设备/点位/规则/告警/账号/审计/运维 API、SSE
│   │   ├── src/north/          北向：mqtt 连接池、encoder（protobuf/json）、runtime
│   │   ├── src/                调度 scheduler、数据面 dataplane、规则 rules、公式 formula、模拟 sim、
│   │   │                       队列 offline_queue、遥测 telemetry_store、审计 audit、告警 alarm、
│   │   │                       控制 ctrl、硬件加固 hardening、指标 metrics、OTA、平台探测 platform …
│   │   ├── src/bin/            iot-daq-daemon（唯一二进制入口）
│   │   ├── tests/              集成测试（授权门控 / 北向接线 / TLS mTLS / 契约一致性 / 压力 / 公式基线）
│   │   └── pki/                OPC UA 客户端证书目录（运行期，不入库）
│   ├── licensing-server/       授权端：http 路由、service 业务、store 持久层、keys/token/receipt、
│   │                           admin_auth、device_auth、audit、model、proto
│   └── protocol-proto/         北向 protobuf：proto/telemetry.proto + build.rs + 往返测试
│
├── web-console/                网关侧控制台（Vue 3；dev 5274；dev 代理 /api → 127.0.0.1:8080）
│   ├── src/api/                client（运行期 API base 解析）/ repo（唯一数据层）/ stream(SSE) / shell(桌面壳桥)
│   ├── src/pages/              18 个页面组件
│   ├── src/guards/             授权端隔离守卫测试（防两端口径回流）
│   └── public/                 favicon / logo 素材
├── admin-console/              厂商后台（Vue 3；dev 5273；dev 代理 /licensing → 127.0.0.1:7080 并剥前缀）
│   ├── src/api/                client（baseURL 硬编码同源前缀 `/licensing`）/ repo
│   ├── src/pages/              12 个页面组件
│   └── .env.production         生产构建：VITE_API_MODE=real
├── ui-kit/                     两端共用（源码级共享，无独立产物）
│   ├── src/components/         17 个共享组件（StatusTag/StatCard/PageHeader/EmptyState/DangerConfirmModal/
│   │                           MachineCodeDisplay/MaskedCode/CodeLifecycleTimeline/RoleGate/Ui*）
│   ├── src/                    rbac / mask / status-map / theme / time / tokens.(ts|css) / icons
│   └── tests/                  vitest（6 文件 / 101 用例）
├── tauri-shell/                Windows 桌面壳（独立 Cargo 工程，脱离根 workspace）
│   └── src-tauri/              main.rs（建窗+拉侧车）/ supervisor.rs（端口探测 8080→8089 + 看门狗）/
│                               tauri.conf.json（NSIS + 资源 + CSP）/ nsis-hooks.nsh（安装目录清理）/ icons/
│
├── deploy/
│   ├── docker/                 网关容器：Dockerfile(+.rendered) / docker-compose.yml / start.sh /
│   │   │                       entrypoint.sh（shell 版，调试用）/ nginx.conf.template + mime.types
│   │   ├── config/gateway.default.toml   容器内默认配置模板
│   │   ├── context/web-console-dist/     ★ 打进镜像的前端产物（跟踪入库，CI 守卫对象）
│   │   └── healthprobe/main.rs           零依赖 TCP 探针（distroless 无 shell）
│   ├── licensing/              授权端单机部署：docker-compose.yml + .env.example
│   ├── nginx/                  宝塔宿主 nginx vhost：iot-daq-licensing.conf（线上落地名 iot-daq-both.conf，`:9013`）
│   │                           + iot-daq-license-domain.conf（公网域名 `license.webscad.cn` 的 `:80` 入口）
│   ├── scripts/                install/uninstall/build-offline-bundle/sign-and-verify/
│   │                           render-dockerfile-digests/check-image-contents/
│   │                           check-web-console-dist/detect-serial + license-e2e/
│   ├── base-images.lock.yaml   基础镜像 tag↔digest 锁文件（禁手写，由渲染脚本注入）
│   └── .env.example            全部环境变量命名权威（唯一入口）
│
├── headless/                   Linux headless 交付形态的占位说明（仅 README.md，尚未建 Cargo 工程）
│
├── .omo/                       计划与会话状态（未入库，禁删）
├── .workbuddy/                 工作区记忆与验收证据（未入库，禁删）
├── docs/                       设计（design/）/ 手册（manual/）/ 运维（handbook/）（未入库，禁删）
├── archive/                    本地历史配置备份归拢处（未跟踪；config-backup/）
├── data/                       运行期数据：audit.db / trial.marker / license/ / 验证日志（未跟踪）
├── pki/                        OPC UA PKI（trusted/ rejected/，未跟踪）
└── target/ target-*/           cargo 构建缓存（未跟踪、可重建；多个 target-* 是并行构建目录）
```

---

## 三、本地启动说明

### 3.0 工具链环境（本机口径，Windows + Git Bash）

```bash
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
export RUSTUP_HOME='D:/rust/rustup'
export CARGO_HOME='D:/rust/cargo'
export PATH="/d/rust/cargo/bin:/d/rust/mingw64/bin:$PATH"
```

- 本机使用 `stable-x86_64-pc-windows-gnu`（winlibs mingw-w64 提供 `gcc` / `dlltool` / `as`）。
- **顺序红线**：`mingw64/bin` 必须在 rustup self-contained 目录**之前**（或直接不引入
  self-contained），否则 `ld` 与 mingw gcc 不匹配，报 `cannot find -lmsvcrt`。
- Rust 版本要求：workspace 声明 `rust-version = "1.85"`；容器基础镜像是 `rust:1.88-slim-bookworm`。

### 3.1 网关端 daemon

```bash
cd /f/xy/iot-daq

# 首次：从模板生成本地配置（config.toml 已被 .gitignore 忽略，严禁入库）
cp config.example.toml config.toml

# 构建 + 运行
cargo build --workspace
cargo run -p daemon --bin iot-daq-daemon -- --config config.toml
```

- **管理面监听 `127.0.0.1:8080`**（默认值，见 `crates/daemon/src/bin/iot-daq-daemon.rs:145`）。
  覆盖方式：`--bind <addr>` 或环境变量 `IOT_DAQ_MGMT_BIND`（亦兼容 `IOT_DAQ_HTTP_BIND`/`IOT_DAQ_HTTP_PORT`）。
  注意：**daemon 自己不做端口顺延**，8080 被占用会直接启动失败——自动换端口是
  **Windows 桌面壳**的能力（`tauri-shell/src-tauri/src/supervisor.rs:74` `pick_mgmt_addr` 从
  8080 顺延探测到 8089，前端经 `api_base` 命令读取实际基址）。
- 静态页面根：`IOT_DAQ_WEB_DIST`（默认 `./web-dist`）；把 `web-console/dist` 指过去即可由 daemon 直接托管。
- 配置文件路径：`--config` 或 `IOT_DAQ_CONFIG`（默认 `./config.toml`；**文件不存在会 fail-fast 退出码 1**）。
- 日志：`IOT_DAQ_LOG_LEVEL`（如 `info`）/ `IOT_DAQ_LOG_JSON=1` 结构化输出。
- 数据根：`IOT_DAQ_DATA_DIR`（默认 `./data`）。
- 容器形态自检：`iot-daq-daemon --preflight`（仅校验配置 / 宿主锚点 / 持久卷并退出：`0` 通过 / `2` 失败）。

**登录凭据**

- **本机现有 `config.toml`** 已配置 `[[mgmt_auth.users]]` → 用户名 **`root`**、角色 `system`；
  口令为 **`admin123`**（仅存在于本机未入库的 `config.toml`，**仓库内无任何明文口令字面量**）。
- **全新环境**（`cp config.example.toml config.toml` 后 `[[mgmt_auth.users]]` 全是注释 ⇒ 零账号）：
  首次访问走 **`POST /api/auth/bootstrap`** 创建首个账号（安全契约：**仅本机回环**可调，
  否则 403；已有账号后恒 409，入口永久关闭）。
- 开发期可用 `IOT_DAQ_DEV_ADMIN_PASS=<pass>` 启用 dev 管理员（username=`dev`，启动日志带 `[WARN]`）；
  `IOT_DAQ_JWT_SECRET=<64位hex>` 指定 JWT 密钥（缺省回退 dev 常量并 `[WARN]`）。**生产禁用。**

### 3.2 授权端 licensing-server

```bash
cd /f/xy/iot-daq
cargo run -p licensing-server
```

- 监听 **`0.0.0.0:7080`**（`IOT_DAQ_LISTEN_ADDR` 覆盖；此 7080 为**本地直跑默认值**，
  容器部署统一用 **9010**，见 §5.2 端口对照）；数据库默认 `./licensing.db`
  （`IOT_DAQ_LICENSE_DB` 覆盖，已被 `.gitignore` 忽略）。
- 管理员凭据（fail-closed：`_SHA256` 与明文都缺 → 登录全拒）：
  `IOTDAQ_ADMIN_USER`（默认 `admin`）、`IOTDAQ_ADMIN_PASSWORD_SHA256`（推荐，恰好 64 位小写 hex，
  `printf '%s' '<口令>' | sha256sum`）、`IOTDAQ_ADMIN_PASSWORD`（明文兜底，摘要优先）。
- 管理端 JWT：`IOTDAQ_JWT_SECRET`（缺省回退内置 dev 密钥并 `warn`，生产必配）。
- Lease Token 私钥：`IOT_DAQ_LICENSE_SIGNING_KID` + `IOT_DAQ_LICENSE_SIGNING_KEY`
  （b64 Ed25519，**必须成对**；缺省时签发端点 fail-closed，验签路径不受影响）。

### 3.3 两个前端

```bash
# 任一前端目录下，先清残留代理（否则 npm/vite 连接复用被劫持，表现为「首请求成功、后续失败」）
export HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy= NO_PROXY='*'

cd /f/xy/iot-daq/web-console   && npm install && npm run dev   # → http://localhost:5274
cd /f/xy/iot-daq/admin-console && npm install && npm run dev   # → http://localhost:5273
```

| 工程 | dev 端口 | dev 代理 | 生产构建 |
|---|---|---|---|
| `web-console` | **5274** | `/api` → `http://127.0.0.1:8080` | `npm run build`（`vue-tsc --noEmit && vite build`） |
| `admin-console` | **5273** | `/licensing` → `http://127.0.0.1:7080`，**转发前剥掉 `/licensing` 前缀** | `npm run build`（`.env.production` 内置 `VITE_API_MODE=real`） |
| `ui-kit` | 无 | 无 | `npx vite build`（仅自验可编译，无发布产物） |

- `ui-kit` 是**源码级共享**（`package.json` `main → src/index.ts`），两端 `vite.config.ts` 与
  `tsconfig.json` 的 `@ui-kit` 别名必须同时指向 `../ui-kit/src`（只配一处会「构建过但类型报错」）。
- 路由为 **hash 路由**，静态托管无需 SPA fallback。
- 测试：`npm run test`（vitest）。

**授权端 baseURL 的前缀陷阱**：`admin-console/src/api/client.ts` 里 `BASE_PATH = '/licensing'` 是
**硬编码同源相对前缀**，而后端路由本身不带前缀（如 `/admin/auth/login`）。因此
**生产环境的宿主 nginx 必须 `proxy_pass http://127.0.0.1:9010/`（带尾斜杠）来剥前缀**
——见 `deploy/nginx/iot-daq-licensing.conf`（线上落地名 `iot-daq-both.conf`）。少写尾斜杠会导致 404。
（另有 `deploy/nginx/iot-daq-license-domain.conf` 提供**公网域名 `license.webscad.cn` 的 80 入口**，
同样剥 `/licensing` 前缀转发到 `127.0.0.1:9010`。）

---

## 四、打包说明

### 4.1 Windows 桌面端（NSIS 安装包）

**前置条件**：Windows 10/11；Rust + target `x86_64-pc-windows-msvc`；Node 22；
NSIS（`makensis` 在 PATH）；WebView2 Runtime（Win11 自带）；
Visual Studio Build Tools（MSVC `cl.exe`，`rusqlite bundled` 需要）。

```bash
cd /f/xy/iot-daq

# ① 先产出 daemon 侧车（★ 必须，否则打包缺资源失败）
#    tauri.conf.json 的 bundle.resources 声明了 resources/iot-daq-daemon.exe
cargo build --release -p daemon --bin iot-daq-daemon
mkdir -p tauri-shell/src-tauri/resources
cp target/release/iot-daq-daemon.exe tauri-shell/src-tauri/resources/

# ② 安装依赖（tauri-cli + web-console）
npm --prefix web-console ci
cd tauri-shell && npm ci

# ③ 本地调试（热重载 web-console）
npm run dev

# ④ 出包（必须在 tauri-shell/ 目录内执行）
npm run build          # = tauri build；beforeBuildCommand 会自动构建 web-console
```

**产物路径**

```
tauri-shell/src-tauri/target/release/bundle/nsis/IoT-DAQ Gateway_0.1.0_x64-setup.exe
```

（版本号取自 `tauri-shell/src-tauri/tauri.conf.json` 的 `version`，**不会**跟随 git tag；
`release-windows.yml` 已加 tag↔version 一致性校验，不一致直接非零退出。）

**`nsis-hooks.nsh` 的作用**：历史版本把 `config.example.toml` 铺到安装目录 `$INSTDIR`，
新版已改为**壳内联生成最小 `config.toml`**（落在 `%APPDATA%\com.iotdaq.gateway\`）。
NSIS 升级安装只覆盖/新增文件、**不会**删除旧版遗留文件，故由
`NSIS_HOOK_POSTINSTALL` / `NSIS_HOOK_POSTUNINSTALL` 显式 `Delete "$INSTDIR\config.example.toml"`。

**CI 自动出包**：`.github/workflows/release-windows.yml`
（推 `v*` tag → 构建 + 创建/更新 GitHub Release 并附 `.exe` 与 SHA256；`workflow_dispatch` 可勾 `sign`，
经仓库 Secrets `WINDOWS_CERTIFICATE` / `WINDOWS_CERTIFICATE_PASSWORD` 做 Authenticode 签名，缺证书自动跳过）。

### 4.2 Linux 端（Docker 容器，主推形态）

**① 渲染 Dockerfile（fail-closed：未 pin 则非零退出）**

`deploy/docker/Dockerfile` 的 `FROM` 是**占位 token**（`__BUILDER_DIGEST__` / `__RUNTIME_DIGEST__`），
**不是可直接构建物**——直接 `docker build -f deploy/docker/Dockerfile` 必然失败，这是刻意设计
（让「未 pin 的构建」无法静默成功）。

```bash
cd /f/xy/iot-daq
./deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
# 从 deploy/base-images.lock.yaml 注入真实 digest（lock 中任一 digest 为空 → 非零退出）
```

**② 校验前端交付目录未被占位页污染**（fail-closed 守卫）

```bash
./deploy/scripts/check-web-console-dist.sh
# 该目录 = deploy/docker/context/web-console-dist/，是 Dockerfile 里 COPY 的源
# CI 会先用真实 web-console 产物 rm -rf 覆盖它，再跑本守卫，最后才 docker build
```

**③ 多架构构建 + 离线包**

```bash
# 在线：buildx 多架构 build + push（需 QEMU/binfmt 才能跨架构）
docker buildx build --platform linux/amd64,linux/arm64 \
  --file deploy/docker/Dockerfile.rendered \
  --tag <registry>/iot-daq/iot-daq-gateway:v1.0.0 --push .

# 离线（主推）：产出 tar + SHA256SUMS（内含渲染 + buildx + docker save 全流程）
IOT_DAQ_IMAGE='<registry>/iot-daq/iot-daq-gateway@sha256:<64hex>' \
IOT_DAQ_VERSION='1.0.0' \
  ./deploy/scripts/build-offline-bundle.sh --env-file deploy/.env
# 产出 deploy/out/：iot-daq-gateway_v1.0.0_{amd64,arm64}.tar / images-digests.txt / SHA256SUMS
```

**④ 镜像层交付物断言**（禁止空层：路径错位会让构建「成功」但层里没东西）

```bash
./deploy/scripts/check-image-contents.sh <image>
```

**关键约定（改动前务必先读 `deploy/README.md §0` 六条红线）**

| 约定 | 说明 |
|---|---|
| 基础镜像 **digest pin** | 禁可变 tag / `:latest`；digest 由 `base-images.lock.yaml` 渲染注入，不手写 |
| **机器码取宿主** | 只读挂载宿主 `/etc/machine-id`、`/sys/class/dmi/id`；`IOT_DAQ_ALLOW_CONTAINER_ANCHORS=0` 禁止回退容器内标识 |
| **授权状态落宿主卷** | `read_only: true` + 唯一 rw 卷 `${IOT_DAQ_DATA_DIR}:/var/lib/iot-daq`；`docker rm && docker run` 不能重置试用 |
| **端口联动** | `IOT_DAQ_WEB_PORT`（对外，默认 8080；**本现场为 9012**，见 §5.2）/ `IOT_DAQ_HTTP_PORT`（容器内 daemon，默认 8081；**本现场为 9011**），由 `deploy/docker/start.sh` 用 `envsubst` 渲染进 nginx 模板；写死其一即会产生「页面能开、API 全 502」 |
| **运行身份固定** | compose 显式 `user: "65532:65532"`，宿主卷与 tmpfs 属主必须对齐（否则 `cap_drop: ALL` 下写拒） |
| **安全** | 非 root、`cap_drop: ALL`、`no-new-privileges`；禁 `--privileged`、禁挂 `/var/run/docker.sock`；镜像层不含密钥/激活码/指纹 |
| 现有基础镜像 | builder `rust:1.88-slim-bookworm`（含 `musl-tools`，编 musl 静态二进制）；runtime `nginx:1.25-alpine`（容器内置 nginx，同时承载静态页与反代） |

---

## 五、部署说明

### 5.1 交付路径总览

```
本地/CI 构建产物 ──(docker save / scp / 可信介质)──▶ 服务器（绝不构建）
                                                      ├── 网关端容器   iot-daq-gateway
                                                      └── 授权端容器   iot-daq-licensing-server
```

**核心纪律：服务器上不构建。** 镜像一律本地/CI `buildx` 构建后
`docker save -o x.tar` → 传输 → `docker load -i x.tar`。

### 5.2 正式服务器（实测现状）

| 项 | 值 |
|---|---|
| 主机 | `60.205.8.146`（root）；公网域名 **`license.webscad.cn` → `60.205.8.146`**（已解析） |
| 系统 | Alibaba Cloud Linux 3 / 4 核 / 内存 ~7.4G（可用 ~2.4G） |
| 容器运行时 | Docker 26.1.3 + Compose v2.27 |
| 宿主 nginx | 由**宝塔 v11.4.1 托管**；vhost 目录 `/www/server/panel/vhost/nginx/*.conf` |
| 端口占用 | `80` / `443` / `888` / `1122` **已被他人既有站点占用**（`lnanyuda.com` 等）——**勿动他人服务** |
| 授权端容器 | `iot-daq-licensing-server`，端口**只绑回环 `127.0.0.1:9010`**，外网经宿主 nginx 反代 |
| 授权端 vhost | `iot-daq-both.conf`（`:9013`，同 IP 直连）与 `iot-daq-license-domain.conf`（`:80`，域名入口） |
| 授权端数据卷 | `/opt/iot-daq/licensing/data`（宿主，须 `chown 65532:65532`） |
| 授权端凭据 | `/opt/iot-daq/licensing/.env`（**不入库**，权限 0600） |
| 网关端容器 | `iot-daq-gateway`（**host 网络**，无 published 端口）：daemon 管理面 `9011`、容器内置 nginx web `9012` |
| 网关端授权基址 | `[gateway.licensing].cloud_url = "http://license.webscad.cn/licensing"`（**域名，不带端口**） |

#### 端口对照：本地开发 vs 线上部署

iot-daq 在线上占用**专用端口段 `9010-9013`**（宿主 `80/443` 已被他人占用）。

| 用途 | 本地开发（默认值） | 线上部署 | 覆盖方式 |
|---|---|---|---|
| 授权端容器监听 | `7080` | **`9010`** | `IOT_DAQ_LISTEN_ADDR`（容器内） |
| 网关 daemon 管理面 | `8080` | **`9011`** | `IOT_DAQ_MGMT_BIND`（或 `IOT_DAQ_HTTP_PORT`） |
| 网关 web（容器内置 nginx） | `8080` | **`9012`** | `IOT_DAQ_WEB_PORT` |
| 授权端 vhost（IP 直连） | — | **`9013`** | 宿主 nginx `iot-daq-both.conf` |
| 授权端域名入口 | — | **`80`**（`license.webscad.cn`） | 宿主 nginx `iot-daq-license-domain.conf` |
| admin-console dev 代理目标 | `127.0.0.1:7080` | — | `admin-console/vite.config.ts`（**不随部署改变**） |

> 口径：代码内 `DEFAULT_LISTEN_ADDR`（授权端）/ daemon 管理面默认端口是**本地直跑**默认值，
> 容器部署一律由 env 覆盖为 `901x`。

### 5.3 授权端（licensing-server）部署

**① 本地构建镜像**（详见 `deploy/licensing/docker-compose.yml` 头部注释）

```bash
./deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/licensing-server.Dockerfile.rendered
docker buildx build --platform linux/amd64 \
  -f deploy/docker/licensing-server.Dockerfile.rendered \
  -t iot-daq-licensing-server:0.1.0 --load .
docker save -o licensing-server-0.1.0.tar iot-daq-licensing-server:0.1.0
```

**② 传输并载入**

```bash
scp licensing-server-0.1.0.tar root@60.205.8.146:/opt/iot-daq/licensing/
ssh root@60.205.8.146 'cd /opt/iot-daq/licensing && docker load -i licensing-server-0.1.0.tar'
```

**③ 服务器上准备目录与 .env**

```bash
mkdir -p /opt/iot-daq/licensing/data && chown -R 65532:65532 /opt/iot-daq/licensing/data
cp deploy/licensing/.env.example /opt/iot-daq/licensing/.env   # 填真实值；.env 严禁入库
# 关键变量：IOTDAQ_ADMIN_USER / IOTDAQ_ADMIN_PASSWORD_SHA256（64 hex）/ IOTDAQ_JWT_SECRET
#           IOT_DAQ_LICENSE_SIGNING_KID + IOT_DAQ_LICENSE_SIGNING_KEY（成对，缺则签发 fail-closed）
#           IOT_DAQ_LISTEN_ADDR / IOT_DAQ_HTTP_PORT（容器部署统一 9010）
```

**④ 启动 + 健康检查**

```bash
cd /opt/iot-daq/licensing && docker compose up -d
docker compose ps        # 应为 Up (healthy)：healthprobe 每 30s TCP connect 容器内 9010
curl -fsS http://127.0.0.1:9010/healthz
```

**⑤ 宿主 nginx（宝塔）挂站点 + 上传前端产物**

```bash
# 站点文件与 vhost 均已在仓库中准备好。⚠️ 仓库文件名 ≠ 线上文件名：
#   deploy/nginx/iot-daq-licensing.conf → 线上落地为 iot-daq-both.conf（:9013）
scp deploy/nginx/iot-daq-licensing.conf root@60.205.8.146:/www/server/panel/vhost/nginx/iot-daq-both.conf
# 公网域名 80 入口（:80，server_name license.webscad.cn）
scp deploy/nginx/iot-daq-license-domain.conf root@60.205.8.146:/www/server/panel/vhost/nginx/
scp -r admin-console/dist/* root@60.205.8.146:/www/wwwroot/iot-daq-licensing/
ssh root@60.205.8.146 'nginx -t && nginx -s reload'
```

两个 vhost 的行为（见文件内注释）：

- `iot-daq-both.conf`：`listen 9013` + `server_name license.webscad.cn 60.205.8.146`（精确匹配，
  不侵入他人站点）；`root /www/wwwroot/iot-daq-licensing`；`location /` 用
  `try_files $uri $uri/ /index.html`（hash 路由，无需 history fallback）；`/assets/` 强缓存 30d；
  `location /licensing/` → `proxy_pass http://127.0.0.1:9010/`（**剥前缀**）。
- `iot-daq-license-domain.conf`：`listen 80` + `server_name license.webscad.cn`，与 `:9013` 站点
  同构同上游，提供**不带端口的公网域名**入口（网关 `cloud_url` 即指向它）。

> 验证：`curl -sS -o /dev/null -w '%{http_code}\n' http://license.webscad.cn/licensing/admin/overview`
> 期望 **401**（无 Bearer）——401 恰证明「域名 → nginx 剥前缀 → 授权端容器」链路是通的。

> 前端必须用 `VITE_API_MODE=real` 构建（`admin-console/.env.production` 已内置），
> 否则页面走 mock 根本不打后端。

### 5.4 网关端（iot-daq-gateway）部署

**① 构建离线包**（厂商侧；也可直接 `buildx` + `docker save` 单架构）

```bash
IOT_DAQ_IMAGE='<registry>/iot-daq/iot-daq-gateway@sha256:<64hex>' \
IOT_DAQ_VERSION='1.0.0' \
  ./deploy/scripts/build-offline-bundle.sh --env-file deploy/.env
# 产出 deploy/out/：*.tar / SHA256SUMS / images-digests.txt（可选 cosign 签名）
```

**② 传输 → 校验 → 载入**

```bash
# 介质转移到现场（如 /opt/iot-daq/offline/），然后：
IOT_DAQ_IMAGE='...@sha256:<64hex>' IOT_DAQ_RELEASE_GIT_SHA=<sha> \
IOT_DAQ_COSIGN_PUBKEY=/opt/iot-daq/offline/cosign/iot-daq-supply.pub \
IOT_DAQ_OFFLINE_BUNDLE_DIR=/opt/iot-daq/offline \
  ./deploy/scripts/sign-and-verify.sh --verify
# 依次执行：sha256sum -c SHA256SUMS → docker load -i <tar> → cosign verify --key <pub> -a git-sha=<sha>
# 任一步失败即停；没有「跳过继续装」开关（R6 红线）
```

**③ 准备配置与环境**

```bash
cp deploy/.env.example deploy/.env     # 填 IOT_DAQ_IMAGE(digest) / IOT_DAQ_DATA_DIR / 串口 / 内存限额 …
./deploy/scripts/detect-serial.sh      # 打印 IOT_DAQ_SERIAL_DEVICES / IOT_DAQ_SERIAL_GROUP 建议值
```

`gateway.toml`（容器内 `/etc/iot-daq/gateway.toml`）需满足**授权可用三要件**：

1. `[gateway.licensing].cloud_url` 必须配置为**授权服务域名基址**
   （生产：`http://license.webscad.cn/licensing`，**不带端口**；宿主 nginx 剥 `/licensing` 前缀后
   转发到 `127.0.0.1:9010`）。未配置 → 授权装配为 `NotConfigured` 被跳过，页面报
   `trial_enabled=false`；
2. 指纹密钥文件（`IOT_DAQ_FINGERPRINT_KEY_FILE`，默认 `/run/secrets/iot-daq-fingerprint-key`）
   **必须真实挂载进容器**，否则装配 fail-closed 且**北向保持关闭**；
3. 该密钥属主 = 容器运行 uid `65532`、权限 `0600`、且落在**持久**路径
   （宿主 `/run` 多为 tmpfs，重启即丢，故用 `/opt/iot-daq/gateway/secrets/...`）。

**④ 一键安装 / 升级**

```bash
sudo ./deploy/scripts/install.sh --bundle /opt/iot-daq/offline/iot-daq-offline-v1.0.0-<arch> --env-file /opt/iot-daq/.env
# 职责：宿主指纹采集 + HMAC → 渲染 compose → 创建并收紧持久卷权限 → docker compose up -d → 健康检查
```

升级 = 换 `IOT_DAQ_IMAGE` 的 **digest** → 重新校验离线包 → `docker compose up -d`；
回滚 = 把 digest 改回旧值再 `up -d`（持久卷不变，授权 / 试用 / 租约 / 配置全部保持）。
卸载 `deploy/scripts/uninstall.sh` 默认**保留持久卷**，删卷需双参数
`--purge-data --yes-i-know-data-is-lost`。

**⑤ 健康判据**

```bash
docker compose ps                                        # gateway = Up (healthy)，观察 60s 无重启抖动
docker compose logs --tail=200                           # 排查用
curl -fsS http://127.0.0.1:${IOT_DAQ_WEB_PORT}/healthz    # 200，body 的 mode 为预期值（新装 = 未激活/试用中）
```

禁止以「删指纹文件 / 改挂载点 / 调参数」的方式绕过启动自检（会被运行期判为环境变更）。

### 5.5 系统更新（OTA）的两种配置方式

授权端 `GET /updates/manifest` 下发升级包清单；网关侧拉取 → **Ed25519 验签** → 版本单调性
（新版本号必须**严格大于** `current_version`）→ 写入**落盘** pending 槽，等待重启确认
（防降级 / 防重放旧包）。**未就绪 = no-op**：只记一条 warn 并跳过，绝不伪造「已是最新」。

**① 配置文件 `[gateway.ota]`**（本地直跑 / 客户环境）

| 键 | 说明 |
|---|---|
| `enabled` | 总开关，缺省 `false` = 不做任何检查、不发网络请求 |
| `manifest_url` | manifest 端点，**可省略**——省略时按 `{gateway.licensing.cloud_url}/updates/manifest` 推导（**推导值不写回配置文件**；canonical：`http://license.webscad.cn/licensing/updates/manifest`） |
| `poll_interval_secs` | 轮询周期（秒，缺省 3600；首个周期后才检查） |
| `signing_key_b64` | 授权端 OTA **Ed25519 公钥** base64（32 字节） |
| `current_version` | 网关当前 OTA 版本号（u64 单调，缺省 `0`） |

**② 容器形态用 env**（根文件系统只读，这是唯一可用的注入通道）

| env | 覆盖 |
|---|---|
| `IOT_DAQ_OTA_ENABLED` | `enabled`（`1`/`true`/`yes`/`on`） |
| `IOT_DAQ_OTA_MANIFEST_URL` | `manifest_url`（**可留空** → 按 `IOT_DAQ_LICENSE_SERVER_URL` 推导） |
| `IOT_DAQ_OTA_SIGNING_KEY_B64` | `signing_key_b64` |
| `IOT_DAQ_OTA_CURRENT_VERSION` | `current_version` |

> 只有 **`enabled` + 有效 manifest 端点（显式，或由 `cloud_url` 推导）+ 公钥**三者齐备，才会真正
> 发起升级检查；缺任一项只记一条 warn 并跳过。实现见 `crates/daemon/src/config.rs` 的
> `OtaSection::is_ready` / `effective_manifest_url`。**当前部署为明文 `http://`**：完整性由 Ed25519
> 验签保证，**机密性无保障**。

---

## 六、约束与红线

- **平台矩阵**：CI 仅 `windows-latest` / `ubuntu-latest`，**不含 macOS**（设计定稿）。
- **纯 Rust 依赖栈**：禁引入需系统 C 库 / cmake / NASM 的依赖（`openssl-sys` 非 vendored、`native-tls`、
  `paho-mqtt`、`aws-lc-rs`）。**vendored C 源允许**——`rusqlite` 的 `bundled` 特性已实测在 mingw 下构建通过。
  新增依赖须过 `cargo deny`（配置见 `deny.toml`）。
- **防逆向 Tier-1**：release 产物 `strip + LTO + panic=abort`（根 `Cargo.toml [profile.release]` 已落实）。
- **密钥红线**：仓库内**不提交**任何激活码 / 私钥 / 真实机器码 / 口令；`config.toml*`、`.env*`、
  `*.pem|key|p12|pfx`、`data/`、`archive/` 均在 `.gitignore` 中。
- **授权判定始终在 Rust 侧**：WebView / JS 只做展示，`RoleGate` 仅控制可见性，不做授权判定。
- **JSON 编码约定**：纳秒时间戳 / uint64 计数器在 JSON 路径必须字符串编码（int64 → string），见 `crates/protocol-proto`。

## 七、常用命令速查

```bash
# 全量门禁（与 CI 同口径）
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check --locked

# 前端门禁
(cd web-console   && npm run build && npm run test)
(cd admin-console && npm run build)
(cd ui-kit        && npm run test)
```

## 八、已知边界

- `docs/`、`.omo/`、`.workbuddy/`、`AGENTS.md`、`CLAUDE.md` **被 `.gitignore` 排除**，不在版本库内。
  因此本 README 对 `docs/design/*.md` 等设计文档的引用，对**外部 clone** 不可达（本仓库内部可用）。
- `target-*`（多个）是各并行 agent 的 cargo 构建目录，属**可重建缓存**，未入库。
  收工后可清理，只保留一个统一 `CARGO_TARGET_DIR`。
- `deploy/docker/context/web-console-dist/` 是**跟踪入库**的前端构建产物（镜像 COPY 输入 + CI 守卫对象），
  不是源码；改前端后需重新构建并覆盖该目录（CI 会自动做，本地需手动）。
- `headless/` 目前只有说明文档，无 Cargo 工程；Linux 的实际交付形态是 `deploy/docker/` 容器镜像。
- `admin-console` 的 `src/mock/mock-data.ts` 是 mock 模式的契约本体（`src/api/repo.ts` 仍在 import），
  **不是死代码**；`VITE_API_MODE=real` 构建时数据层走真实接口。
