# iot-daq 网关 · 部署手册（生产级 / 可离线部署）

> 适用版本：v1.0.0（首发 amd64 + arm64）
> 配套设计文档（权威依据，冲突以设计文档为准）：
> - `docs/design/container-deploy.md` — 运行参数 / 设备接入 / 离线流程
> - `docs/design/container-machine-binding.md` — 宿主指纹锚点（陷阱 1）
> - `docs/design/container-persistence-layout.md` — 持久卷布局（陷阱 2）
> - `docs/design/container-supply-chain.md` — 镜像签名与 digest pin
> - `docs/design/installer-hardening.md` — 安装程序防破解（镜像 = 分发物）
> - `deploy/.env.example` — 全部环境变量命名权威

本手册覆盖两类交付形态：

1. **在线交付**：厂商 CI 多架构 `buildx build --platform linux/amd64,linux/arm64` + push 到内网/厂商 registry，现场 `docker compose up -d` 拉取。
2. **离线交付（主推）**：本仓库 `deploy/` 目录产出离线 tar 包 + 校验脚本，经可信介质转移到无外网现场，载入后一键安装。

---

## 0. 红线（贯穿全文，违反即视为缺陷）

| # | 红线 | 理由 / 替代 |
|---|------|------------|
| R1 | 镜像引用一律 **digest pin**（`repo@sha256:<64hex>`），**禁可变 tag / `:latest`** | 可变 tag 指向内容可被上游静默替换，信任链首环失控。compose 与 `.env` 的 `IOT_DAQ_IMAGE` 必须是 digest 形式（`container-supply-chain.md` §3/§6）。 |
| R2 | **绝不**在镜像层 / 交付物内放入任何密钥、激活码、指纹盐、KMS 凭据、客户配置 | 镜像可导出解包等同公开；敏感物一律运行时注入或落宿主卷（`container-persistence-layout.md` §2）。 |
| R3 | **禁 `--privileged`**、**禁挂 `/var/run/docker.sock`** | 全开能力面 / 变相 root 远超业务所需，且破坏「容器不知自己是容器」的指纹原则（`container-deploy.md` §5）。串口/网络能力已最小化声明。 |
| R4 | `read_only: true` + **唯一** rw 持久卷；授权/试用/租约必须落宿主卷 | 否则 `docker rm && docker run` 即「重置试用」后门（陷阱 2）。 |
| R5 | 宿主机器码**只取自宿主机**（陷阱 1） | 容器内 machine-id / 容器 MAC / 主机名每次重建即变 → 授权失效 + SQLCipher 解不开。 |
| R6 | 供应链校验失败即停，**无「跳过继续装」开关** | `container-supply-chain.md` §7/§9。 |

> 任何脚本（含本目录 `deploy/scripts/*.sh`）内**不包含任何真实密钥/口令/激活码/指纹盐字面量**，敏感值仅从环境变量读取。

---

## 1. 环境前置

### 1.1 宿主要求

- Linux（x86_64 工控机 / 虚拟机，或 ARM64 网关盒如 RK3568、树莓派 CM4 类）。
- 已安装 Docker Engine（≥ 20.10）与 `docker compose` v2 插件。
- 现场串口设备已插好（Modbus RTU / RS-485 经 USB 转串口或原生串口）。
- 若需离线签名校验：`cosign` 二进制（v2.x）。缺 cosign 时见 §4 降级路径（不推荐）。
- 建议专用系统账户运行服务（如 `iotdaq`，uid/gid 990），**不要用 root、不要用登录账户**。

### 1.2 准备 .env（现场零改 compose，只填 .env）

```bash
cp deploy/.env.example deploy/.env
# 按现场实际情况编辑 deploy/.env（变量命名见 .env.example 注释）
```

关键变量（节选，完整见 `.env.example`）：

| 变量 | 含义 | 红线关联 |
|------|------|---------|
| `IOT_DAQ_IMAGE` | 镜像 digest 引用，如 `iot-daq/iot-daq-gateway@sha256:<64hex>` | R1（必为 digest） |
| `IOT_DAQ_COSIGN_PUBKEY` | 验签公钥路径（独立渠道分发） | R6 |
| `IOT_DAQ_RELEASE_GIT_SHA` | 发布 git-sha（`-a` 断言） | R6 |
| `IOT_DAQ_OFFLINE_MODE` | 置 1 跳所有外网动作 | 离线现场 |
| `IOT_DAQ_ALLOW_VERIFY_DEGRADE` | 默认 0；置 1 允许无 cosign 降级 | R6 |
| `IOT_DAQ_DATA_DIR` | 宿主持久卷根（授权/试用/租约/队列/配置/日志） | R4 |
| `IOT_DAQ_SERIAL_DEVICES` / `IOT_DAQ_SERIAL_GROUP` | 串口设备与组 | §7 |

---

## 2. 在线交付（buildx 多架构 build + push）

适用于现场可访问内网/厂商 registry 的场景。

```bash
# 在厂商 CI 构建机执行（需 QEMU/binfmt 以支持跨架构构建）
docker buildx create --use --name iot-daq-builder 2>/dev/null || true

# ① 基础镜像 digest 注入：Dockerfile 的 FROM 为占位 token，需先从 lock 渲染
#    （lock 中任一 digest 为空 → 脚本非零退出 = fail-closed，未 pin 的构建不会静默成功）
./deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered

docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --file deploy/docker/Dockerfile.rendered \
  --tag registry.vendor-internal.example/iot-daq/iot-daq-gateway:v1.0.0 \
  --provenance=true --sbom=true \
  --push \
  registry.vendor-internal.example/iot-daq/iot-daq-gateway:v1.0.0

# 取 manifest digest 写入 .env 的 IOT_DAQ_IMAGE（R1）
docker buildx imagetools inspect \
  registry.vendor-internal.example/iot-daq/iot-daq-gateway:v1.0.0 \
  | grep -A1 'Name:'   # 记录 sha256:... 填入 IOT_DAQ_IMAGE

# 对镜像做 cosign 签名（私钥经 KMS，见 §4 / supply-chain §4）
IOT_DAQ_IMAGE='...@sha256:<digest>' IOT_DAQ_RELEASE_GIT_SHA=<sha> \
IOT_DAQ_COSIGN_KEY=kms://vendor-kms/iot-daq/cosign-supply \
  ./deploy/scripts/sign-and-verify.sh --sign
```

现场：

```bash
docker compose --env-file deploy/.env -f deploy/docker/docker-compose.yml up -d
```

---

## 3. 离线交付全流程（主推）

### 3.1 厂商侧：构建离线包

```bash
# 在厂商 CI 构建机执行（产出到 deploy/out/）
IOT_DAQ_IMAGE='registry.vendor-internal.example/iot-daq/iot-daq-gateway@sha256:<64hex>' \
IOT_DAQ_VERSION='1.0.0' \
  ./deploy/scripts/build-offline-bundle.sh --env-file deploy/.env

# 如需构建后立即签名：
IOT_DAQ_COSIGN_KEY=kms://vendor-kms/iot-daq/cosign-supply \
  ./deploy/scripts/build-offline-bundle.sh --env-file deploy/.env --sign
```

产出（`deploy/out/`）：

```
iot-daq-gateway_v1.0.0_amd64.tar      # docker save 产物（单架构）
iot-daq-gateway_v1.0.0_arm64.tar
images-digests.txt                    # 各架构真实 digest 记录
SHA256SUMS                            # 全部随包文件哈希清单
cosign-pubkey.pem                     # 仅当提供 IOT_DAQ_COSIGN_PUBKEY 时生成
```

> 单架构现场可 `./build-offline-bundle.sh --arch arm64` 节省体积。

### 3.2 介质转移

将 `deploy/out/` 整体复制到可信离线介质（加密 U 盘 / 内网传书），转移到现场宿主 `/opt/iot-daq/offline/`。

> **公钥分发纪律**（supply-chain §4.1）：cosign 验签公钥应经**独立于安装包**的渠道（手册附公钥指纹卡片 + 厂商站点公示）分发，防止「篡改包 + 换公钥」双篡改。切勿把公钥与安装包放在同一处。

### 3.3 现场侧：校验（`sign-and-verify.sh --verify`）

```bash
IOT_DAQ_IMAGE='...@sha256:<64hex>' \
IOT_DAQ_RELEASE_GIT_SHA=<sha> \
IOT_DAQ_COSIGN_PUBKEY=/opt/iot-daq/offline/cosign/iot-daq-supply.pub \
IOT_DAQ_OFFLINE_BUNDLE_DIR=/opt/iot-daq/offline \
  ./deploy/scripts/sign-and-verify.sh --verify
```

`--verify` 依次执行（任一步失败即停）：

1. `sha256sum -c SHA256SUMS` —— 离线包完整性；
2. `docker load -i <tar>` —— 导入镜像；
3. `cosign verify --key <pubkey> -a git-sha=<sha> <image>` —— 来源真实性 + git-sha 断言。

> 缺 cosign 且 `IOT_DAQ_ALLOW_VERIFY_DEGRADE=0`（默认）即失败。仅当显式置 1 才走降级（仅哈希 + load + 醒目警告 + 记残余风险于 `RESIDUAL_RISK.verify.log`），**不推荐**。

### 3.4 宿主指纹采集（陷阱 1 前置）

离线包应随附 `host-fingerprint-collect.sh`（由安装脚本在宿主采集首个物理网卡 MAC，排除虚拟/veth 接口，产出 HMAC 签名 `host-fingerprint.json`）。安装脚本 `install.sh` 会调用并写入 `.env` 的 `IOT_DAQ_HOST_MAC`，再以只读挂载进容器。容器**绝不自行采集**。

### 3.5 一键安装

```bash
sudo ./deploy/scripts/install.sh \
  --bundle /opt/iot-daq/offline/iot-daq-offline-v1.0.0-<arch> \
  --env-file /opt/iot-daq/.env
```

`install.sh` 负责：指纹采集 + HMAC、渲染 compose、创建并收紧持久卷权限、`docker compose up -d`、健康检查。**本目录脚本不重复这些职责。**

### 3.6 健康检查

判据（与 `container-deploy.md` §3.1 一致）：

1. `docker compose ps` 显示 `gateway` 为 `running`（观察 60s 无重启抖动）；
2. 容器内健康检查（compose `healthcheck`）：调用镜像内置静态探针 `/usr/local/bin/healthprobe`（纯 std 零依赖，源码 `deploy/docker/healthprobe/main.rs`），每次执行一次 TCP connect `127.0.0.1:${IOT_DAQ_HTTP_PORT:-8080}`，成功 exit 0 / 失败 exit 1。runtime 镜像为 distroless/static（无 shell、无 curl/wget），**不能**使用 `CMD-SHELL` + `/dev/tcp` 形式的探针；
3. 宿主 `curl -fsS http://127.0.0.1:8080/healthz` 返回 200，body `mode` 为预期值（新装=未激活/试用中）；
4. 未通过：`docker compose logs --tail=200` 与 `docker inspect --format '{{json .State.Health}}' <容器>` 排查；**禁止**以删指纹文件 / 改挂载点方式绕过（会被运行时自检判为环境变更）。

---

## 4. digest pin 与 cosign 验签要点

- **digest pin（R1）**：`IOT_DAQ_IMAGE` 与 compose `image:` 必须 `@sha256:` 形式。可变 tag 在 `build-offline-bundle.sh`、`sign-and-verify.sh` 中会被**主动拒绝**。
- **cosign key-pair（采用）**：私钥由厂商 KMS 托管（与授权签发私钥职责分离，均不进镜像/CI明文/git）。现场仅需公钥即可离线验签（`container-supply-chain.md` §4.1）。
- **校验三步串联**（任一失败即拒）：
  1. digest 一致：待装 digest == tar 清单 digest == compose 引用 digest；
  2. 签名有效：`cosign verify --key <pubkey> -a git-sha=<sha>`；
  3. attestation 存在（可选）：`cosign verify-attestation --type spdxjson`（SBOM）。
- **签名命令**（厂商侧，`sign-and-verify.sh --sign`）：
  - key-pair：`cosign sign --key <kms/key> -a git-sha=<sha> <image@digest>`
  - keyless（仅在线 CI，离线现场不可用）：`cosign sign --yes <image@digest>`
- **验签命令**（现场，`sign-and-verify.sh --verify`）：见 §3.3。
- **基础镜像同样 digest pin**：`Dockerfile` 内 `FROM` 一律 `@sha256:` 形式，**digest 不手写**——由 `deploy/scripts/render-dockerfile-digests.sh` 从 `deploy/base-images.lock.yaml` 渲染注入（supply-chain §3）。
  - lock 文件记录「基础镜像 tag ↔ digest ↔ 锁定日期 ↔ 审批人」，digest 变更视为受审变更；
  - Dockerfile 的 `FROM` digest 段为**占位 token**（`__BUILDER_DIGEST__` / `__RUNTIME_DIGEST__`），因此**不是可直接构建物**；直接 `docker build -f deploy/docker/Dockerfile` 会因占位非法而失败（**fail-closed**，让未 pin 的构建无法静默成功）；
  - 构建前先渲染（CI / 本机均可）：
    ```bash
    deploy/scripts/render-dockerfile-digests.sh --output deploy/docker/Dockerfile.rendered
    # 再看 health message：lock 中任一 digest 为空 → 脚本非零退出（fail-closed）
    deploy/scripts/render-dockerfile-digests.sh --check   # CI 门禁：仅校验结构/格式
    ```
  - `deploy/scripts/build-offline-bundle.sh` 已内置该渲染步骤（从 lock 注入 digest 后再 buildx）。
  - 当前锁定的基础镜像（2026-09-25，D-04/D-09b 受审变更）：
    - builder：`rust:1.88-slim-bookworm`——Cargo.lock 传递依赖要求 rustc ≥ 1.88，1.85 容器内编译必失败；builder 内安装 `musl-tools`，以 `--target <musl triple> -C target-feature=+crt-static` 编出**完全静态**二进制；
    - runtime：`gcr.io/distroless/static-debian12:nonroot`——无 shell / 无 loader，要求二进制 musl 静态链接（与 builder 产物匹配）。若未来改用 glibc 动态二进制，必须换 `distroless/cc-debian12` 并同步 Dockerfile 注释与本手册。
- **`latest` 禁止出现在任何交付物中**（supply-chain §2.2）。

---

## 5. 宿主指纹注入（陷阱 1）

**核心**：容器内机器码只来源宿主机；容器重建 / 升级 ≠ 换机；宿主重装系统 = 换机（走后台废弃 + 重发）。

随包 compose 固化（只读挂载，带 `/host` 前缀避免与镜像内 machine-id 混淆）：

```yaml
volumes:
  - ${IOT_DAQ_HOST_MACHINE_ID:-/etc/machine-id}:/host/etc/machine-id:ro
  - ${IOT_DAQ_HOST_DMI_DIR:-/sys/class/dmi/id}:/host/sys/class/dmi/id:ro
  - ${IOT_DAQ_FINGERPRINT_FILE:-/var/lib/iot-daq/host-fingerprint.json}:/var/lib/iot-daq/host-fingerprint.json:ro
environment:
  IOT_DAQ_HOST_MAC: ${IOT_DAQ_HOST_MAC:-}      # 宿主 MAC，安装脚本采集注入
  IOT_DAQ_ALLOW_CONTAINER_ANCHORS: "0"          # 严禁回退到容器内标识
```

- 锚点来源（≥3，取宿主）：宿主 `machine-id`、`/sys/class/dmi/id` 的 `product_uuid/board_serial`、安装脚本采集的首个物理网卡 MAC（HMAC 签名 `host-fingerprint.json`）。
- 容器内 `/etc/machine-id`、`veth MAC`、容器主机名**一律禁用**为锚点（重建即变 → 授权失效 + SQLCipher 解不开）。
- **降级**：精简宿主缺 machine-id/DMI 时，走 `host-fingerprint-collect.sh` 生成签名指纹文件（machine-binding §3）。
- 安装脚本交互式写入指纹 HMAC 盐到 `/run/secrets/iot-daq-fingerprint-key`（权限 0600，属主=服务账户），**绝不进镜像层 / git**（installer-hardening §1）。

---

## 6. 持久卷（陷阱 2）

**核心**：镜像可写层承载的授权/业务数据清单为「空」；一切状态落宿主持久卷，使 `docker rm && docker run` 无法重置试用/租约。

```yaml
read_only: true                 # 镜像可写层只读（重置试用在物理上不可能）
tmpfs:                          # 仅临时文件，重建即清，绝不放授权状态
  - /tmp:rw,noexec,nosuid,size=64m,mode=1777
  - /run:rw,noexec,nosuid,size=16m,mode=0755
  - /home/nonroot:rw,noexec,nosuid,size=16m,mode=0700
volumes:
  - ${IOT_DAQ_DATA_DIR:-/var/lib/iot-daq}:/var/lib/iot-daq:rw   # ★ 唯一持久根（rw）
```

持久卷布局（`/var/lib/iot-daq/`，见 persistence-layout §1）：

```
/var/lib/iot-daq/
├── queue.db            # 断网续传队列（必须持久）
├── telemetry.db        # 遥测库
├── config/             # 客户配置（点位表/北向出口/告警）
├── logs/               # 运行日志（本地轮转；审计/授权日志不走 stdout）
├── license/            # 授权凭证（Lease Token/激活凭证，加密）
├── trial/              # 试用标记（多重冗余之一，加密）
└── host-fingerprint.json  # 宿主指纹（安装脚本生成，只读语义）
```

- 授权 / 试用 / 队列 / 配置 / 日志**五类全部落宿主卷**；容器内路径与宿主一致挂载。
- **重建语义**：同卷重建 → 试用/租约/授权/队列/配置全部保持；空卷重建 → 视为新装但宿主指纹相同 → 云端识别同设备 + 云端首激时间兜底 → 「换容器重置试用」不成立（persistence-layout §3）。
- 卸载（`uninstall.sh`）默认保留持久卷；删卷需显式 `--purge-data --yes-i-know-data-is-lost`（双参数，不可逆）。

---

## 7. 串口与 host 网络映射

### 7.1 串口（南向 Modbus RTU / RS-485）

先跑探测脚本生成建议配置（对应 `.env.example` §7 第 97 行）：

```bash
./deploy/scripts/detect-serial.sh
# 输出形如：
# IOT_DAQ_SERIAL_DEVICES=/dev/serial/by-id/usb-FTDI_...
# IOT_DAQ_SERIAL_GROUP=dialout
```

将这两行写入 `deploy/.env` 替换 §7 默认值。compose 以 `--device` 精确放行 + `group_add` 匹配宿主串口组属主，**替代** `--privileged`：

```yaml
devices:
  - ${IOT_DAQ_SERIAL_DEVICES:-/dev/ttyUSB0}
group_add:
  - ${IOT_DAQ_SERIAL_GROUP:-dialout}
```

- 声明几个开几个；未插线的节点会导致容器启动失败。
- 换口（ttyUSB 编号漂移）建议改用稳定路径 `/dev/serial/by-id/...`（detect-serial.sh 优先推荐）。
- `detect-serial.sh` 同时打印 `dialout`/`tty` 组成员与节点属主，便于排障权限。

### 7.2 host 网络（Modbus TCP 广播发现 / UDP 组播必需）

```yaml
network_mode: host
```

- host 模式下容器与宿主共享协议栈，广播/组播不经 docker-proxy 失真（`container-deploy.md` §2.2）。
- host 网络下**不要写 `ports:`**（与 `network_mode` 冲突）。
- 管理面监听 `0.0.0.0:8080`；建议仅绑定内网网卡 + 宿主防火墙（ufw/firewalld）放行，不在 compose 内改绑。
- 容器内进程仅监听业务端口，不新增监听；无需 `NET_ADMIN`，仅按需 `NET_BIND_SERVICE`（绑定 <1024 端口如 502 时）。

---

## 8. 资源限制与日志

```yaml
mem_limit: ${IOT_DAQ_MEM_LIMIT:-512m}
cpus: ${IOT_DAQ_CPUS:-1.0}
restart: unless-stopped
logging:
  driver: json-file
  options: { max-size: "${IOT_DAQ_LOG_MAX_SIZE:-20m}", max-file: "${IOT_DAQ_LOG_MAX_FILE:-5}" }
```

- 资源限制约束采集进程异常（死循环/内存泄漏）不拖垮宿主（≤8 设备用默认值）。
- `restart: unless-stopped`：断电/宿主重启自愈；人工 `docker stop` 后不违反运维意图。
- 日志驱动本地轮转（上限约 100MB），防写爆宿主盘（无人值守现场）。
- **审计/授权日志不走 stdout**，落持久卷 `logs/`，由应用自身滚动（避免被日志驱动截断丢失）。

---

## 9. 升级与回滚

- 镜像**不可变**，OTA/升级只更新应用产物（task 35），不动镜像与宿主锚点。
- 升级流程：厂商发布新 digest → 更新 `.env` 的 `IOT_DAQ_IMAGE`（digest）→ 新离线包 `build-offline-bundle` + `sign-and-verify --verify` → `docker compose up -d`（自动拉取新 digest）。
- 回滚：将 `IOT_DAQ_IMAGE` 改回旧 digest（旧版本镜像保留可重建，supply-chain §3.3），重新 `up -d`。持久卷不变 → 授权/试用/租约/配置全部保持。
- 升级失败：容器因入口校验（daemon `--preflight`，distroless 镜像内无 shell，校验由二进制承担）拒绝启动 → 查 `docker compose logs`；**禁止**删指纹文件重生成绕过。

---

## 10. 备份与恢复

- **必备份**：`/var/lib/iot-daq`（含 `license/`、`trial/`、`queue.db`、`config/`、`logs/`、`host-fingerprint.json`）。这是授权/状态唯一真相源。
- 备份方式（宿主侧，容器运行或停止均可，停容器更一致）：
  ```bash
  sudo tar czf iot-daq-data-$(date +%F).tgz -C /var/lib iot-daq
  ```
- **恢复**：停容器 → 还原 `/var/lib/iot-daq` → 保持属主/权限（服务账户，750）→ `up -d`。
- 注意：`host-fingerprint.json` 由安装脚本按宿主生成（含 HMAC 签名）；换机恢复需重新采集，不可直接拷贝他机指纹文件（视为伪造）。
- 镜像本身无需备份（随包 tar + digest 可重建）。

---

## 11. 排障 FAQ

| 现象 | 原因 / 处置 |
|------|------------|
| 容器起不来，入口校验（daemon `--preflight`）报「锚点 machine-id 与容器自身相同」 | 挂载落回容器内路径（陷阱 1）。改用随包 compose 或 `diagnose-anchors.sh` 定位（shell 版 `entrypoint.sh` 对应错误码 21；容器内 `--preflight` 统一 exit 2，原因见日志）。 |
| 容器起不来，报「持久卷不可写 / 缺失」 | 卷未挂载或 `:ro` / 属主不符（陷阱 2）。`mkdir -p` + `chown` 服务账户 + 确认 compose 保留 rw 挂载（错误码 30/31）。 |
| 串口设备节点打不开（permission denied） | 当前用户不在 `dialout` 组，或 `group_add` 组名与节点组属主不符。`detect-serial.sh` 查属主；把服务账户加入对应组。 |
| `cosign verify` 失败 | 镜像被篡改/来源不可信，或公钥与签名不匹配。整包作废，从厂商渠道重取；核对公钥指纹（R6）。 |
| 缺 cosign 二进制无法校验 | `IOT_DAQ_ALLOW_VERIFY_DEGRADE=0` 默认失败；需部署 cosign。仅在应急且明确残余风险时置 1（§3.3 / §4）。 |
| `IOT_DAQ_IMAGE` 用 `:latest`/可变 tag 报错 | R1 红线。`build-offline-bundle.sh` / `sign-and-verify.sh` 主动拒绝；改为 `@sha256:` digest。 |
| 构建报「基础镜像 digest 未锁定 / render fail-closed」 | `deploy/base-images.lock.yaml` 中对应 image 的 `digest` 为空。在具备网络的环境解析真实 digest（`docker buildx imagetools inspect <repo>:<tag> --format '{{.Manifest.Digest}}'`）填入后重试；此即「未 pin 的构建不可能静默成功」的设计意图。 |
| 健康检查 `/healthz` 返回非 200 | 看 `docker compose logs`；多为授权模块/挂载问题，**不要**改参数绕过（会被自检判为环境变更）。 |
| 卸载后授权丢失 | `uninstall.sh` 默认保留数据；只有 `--purge-data` 才删卷。误删需从备份恢复（§10）。 |

---

## 12. 交付物清单（本目录）

| 文件 | 作用 |
|------|------|
| `deploy/docker/Dockerfile` | 多阶段构建（digest pin 基础镜像，非 root，镜像层无敏感物） |
| `deploy/base-images.lock.yaml` | **本手册新增**：基础镜像 tag ↔ digest 锁文件（digest 由 CI 渲染注入，禁手写） |
| `deploy/scripts/render-dockerfile-digests.sh` | **本手册新增**：从 lock 渲染 Dockerfile 的 `FROM ...@sha256:` 行；digest 未锁定即 fail-closed |
| `deploy/docker/docker-compose.yml` | 随包唯一事实源（digest 镜像、陷阱 1/2 固化、host 网络、资源限制） |
| `deploy/docker/entrypoint.sh` | 宿主锚定 + 持久卷可写性前置校验（fail-fast）。**不拷入 distroless runtime 镜像**（D-15：无 shell 下不可执行，容器内校验由 daemon `--preflight` 承担）；仅用于原生部署 / 调试变体（cc/shell）/ 现场排障 |
| `deploy/docker/config/gateway.default.toml` | 默认配置模板（占位值，无凭据） |
| `deploy/.env.example` | 全部环境变量命名权威（含红线注释） |
| `deploy/scripts/install.sh` | 宿主一键安装（指纹采集 + HMAC + 渲染 compose + up -d + 健康检查） |
| `deploy/scripts/uninstall.sh` | 卸载（默认保留持久卷，删卷需双参数） |
| `deploy/scripts/build-offline-bundle.sh` | **本手册新增**：构建多架构镜像 + 导出 tar + SHA256SUMS + 可选签名 |
| `deploy/scripts/sign-and-verify.sh` | **本手册新增**：`--sign` 签名 / `--verify` 离线校验（含降级路径） |
| `deploy/scripts/detect-serial.sh` | **本手册新增**：探测串口设备并生成 `IOT_DAQ_SERIAL_DEVICES` 建议配置 |
| `deploy/README.md` | **本手册新增**：本部署手册 |

> 本手册三类新增脚本均满足：bash、`set -euo pipefail`、中文注释、可执行、无硬编码密钥、`bash -n` 静态自检通过。
