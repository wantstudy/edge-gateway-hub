# 容器镜像供应链安全（task 59 · 补充设计）

> 归属：`iot-daq` Wave 0 设计基线 · 计划任务 59（容器化交付）· 2026-09-23 定稿
> 前置阅读：`container-delivery.svg`（四层拓扑）、`container-machine-binding.md`（宿主锚点）、`installer-hardening.md`（task 43 分发物防篡改）

## 1. 目标与范围

镜像供应链安全是授权体系的**前置防线**：若客户现场运行的镜像被替换为篡改版（剥离验签逻辑、伪造租约），task 40-44 的全部本地防护即失效。本文件定义从构建到现场落地的完整校验链，保证「现场跑的 = 厂商构建的」。与 task 43 的区别：task 43 保护**安装包/试用标记**，本文保护**容器镜像本体与元数据**。

覆盖：多架构构建、基础镜像 digest pin、cosign 签名与校验链、SBOM、compose digest 引用、离线 tar 校验脚本、私有 registry 可选方案。

## 2. 多架构构建（buildx）

### 2.1 目标架构

| 架构 | 典型现场 | 优先级 |
|------|---------|--------|
| `linux/amd64` | x86 工控机、虚拟机网关 | P0（首发） |
| `linux/arm64` | ARM 网关盒（RK3568/树莓派 CM4 类，工业现场常见） | P0（首发） |

不做 `armv7`（32 位）与 `windows/amd64`（Windows 走 Tauri 原生安装包，见 `container-deploy.md` 不做清单）。

### 2.2 构建命令约定

```bash
# CI 中执行；输出为多架构 manifest list（一次 build，两架构共享 tag/digest 骨架）
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --file Dockerfile \
  --tag iot-daq-gateway:v1.0.0 \
  --provenance=true --sbom=true \
  --push \
  registry.vendor-internal.example/iot-daq/iot-daq-gateway:v1.0.0
```

要点：
- 构建产物是 **manifest list**，`docker pull` 时按宿主架构自动选取；离线 tar 分发时需**按架构分别导出**（见 §7）。
- `--provenance --sbom` 由 buildx 附加构建证明与 SBOM attestation，随镜像一同可被 cosign 校验。
- 版本 tag 规则：`vMAJOR.MINOR.PATCH`，另附 `vX.Y.Z-<git短哈希>` 便于审计回溯；`latest` **禁止出现在任何交付物中**。

## 3. 基础镜像 digest pin（禁可变 tag）

### 3.1 规则

- Dockerfile 中 `FROM` 一律使用 **digest 引用**，禁止 `FROM rust:latest`、`FROM debian:bookworm` 等可变 tag：
  ```dockerfile
  FROM rust:1.79-slim@sha256:6c3c6ce...   # 示例格式，实际以 CI 锁定为准
  FROM debian:bookworm-slim@sha256:1c7e9f8...
  ```
- digest 由 CI 在依赖升级流水线中统一刷新，**人工不得在业务 Dockerfile 内手写 digest**。
- 锁定文件：仓库内 `deploy/base-images.lock.yaml` 记录「基础镜像 tag ↔ digest ↔ 锁定日期 ↔ 审批人」，digest 变更视为一次受审变更。

### 3.2 理由

- 可变 tag 指向的内容可被上游静默替换，等于把信任链首环交给第三方 registry 的滚动指针。
- digest pin 后，同 digest 内容不可变，离线现场（无外网）导入的镜像与 CI 构建的镜像逐字节可比对。

### 3.3 升级流程

1. 定期（建议每月）由 CI 任务 `renovate`/自研脚本扫描基础镜像新 digest + CVE 报告；
2. 提交 lock 文件变更 → 重构建 → 重签名 → 走既有灰度发布；
3. 老版本镜像不删除，保持「已交付版本可重建」。

## 4. cosign 签名与校验链

### 4.1 签名方式选型

| 方案 | 结论 | 理由 |
|------|------|------|
| cosign keyless（Fulcio/Rekor） | **不采用** | 现场无外网，无法访问公共 CA 与透明日志 |
| cosign key-pair | **采用** | 私钥由厂商 KMS 托管，现场仅需公钥即可离线验签 |

- 签名私钥与 task 44 的授权签发私钥**分属两把**（职责分离：供应链私钥泄露不波及授权体系，反之亦然），均仅存 KMS（HSM 优先），不入镜像、不入 CI 明文缓存、不入 git。
- 现场验签公钥随**安装介质外**的独立渠道分发（安装手册附公钥指纹卡片 + 校验站网页公示），防「篡改包 + 换公钥」双篡改。

### 4.2 签名（厂商侧 CI）

```bash
cosign sign --key kms://vendor-kms/iot-daq/cosign-supply \
  -a git-sha=$GIT_SHA -a build-id=$BUILD_ID \
  registry.vendor-internal.example/iot-daq/iot-daq-gateway:v1.0.0
# 同时对 SBOM attestation 签名
cosign attest --key kms://vendor-kms/iot-daq/cosign-supply \
  --predicate sbom.spdx.json --type spdxjson \
  registry.vendor-internal.example/iot-daq/iot-daq-gateway:v1.0.0
```

### 4.3 校验链（现场 / CI 双用）

完整校验 = **三步串联**，任一步失败即拒绝安装：

1. **digest 一致**：待装镜像 digest == tar 清单声明的 digest == compose 文件引用的 digest；
2. **签名有效**：`cosign verify --key iot-daq-supply.pub <镜像>`，断言含 `-a git-sha` 且与发布记录一致；
3. **attestation 存在**：`cosign verify-attestation --type spdxjson` 能取回 SBOM。

```bash
cosign verify --key iot-daq-supply.pub \
  -a git-sha=<发布记录中的SHA> \
  iot-daq-gateway@sha256:<digest>
```

CI 出厂门禁与现场 `verify.sh` 走**同一条命令序列**，校验逻辑单一实现。

## 5. SBOM 生成与随包交付

- 生成：CI 以 `syft <镜像> -o spdx-json > sbom-vX.Y.Z.spdx.json` 产出（buildx `--sbom` attestation 作为镜像侧冗余）；
- 交付：SBOM 文件 + 其 SHA256 一并放入离线 tar 包（见 §7），并在厂商站点按版本归档可下载；
- 用途：客户等保/审计要求、CVE 爆发时快速定位受影响版本（SBOM 含基础镜像层与依赖清单）；
- SBOM 本身不做完整性锚，完整性由 tar 清单中的 SHA256 保证。

## 6. compose 以 digest 固定镜像引用

随包 `docker-compose.yml` 中镜像引用一律为 digest 形式，**不含 tag**：

```yaml
services:
  gateway:
    image: iot-daq/iot-daq-gateway@sha256:<digest>   # 由 CI 在打包阶段注入
    ...
```

- 打包脚本从 `cosign download` / buildx 输出读取 manifest list digest 写入 compose；人工不改 compose 内镜像行。
- 效果：现场 `docker compose up -d` **只能**加载 tar 中那份镜像，本地即使存在同 tag 其他镜像也不会被误用。

## 7. 离线 tar 包哈希清单 + 校验脚本

### 7.1 包结构（`iot-daq-offline-vX.Y.Z-<arch>.tar.gz`）

```
iot-daq-offline-v1.0.0-arm64/
├── images/
│   └── iot-daq-gateway_v1.0.0_arm64.tar        # docker save 产物（单架构）
├── docker-compose.yml                           # digest 引用，CI 注入
├── host-fingerprint-collect.sh                  # 宿主锚点采集脚本（见 container-machine-binding.md）
├── sbom/sbom-v1.0.0.spdx.json
├── cosign/iot-daq-supply.pub                    # 供应链验签公钥
├── checksums.sha256                             # 全部文件的 SHA256 清单
└── verify.sh                                    # 安装前校验脚本
```

### 7.2 verify.sh（无外网现场的安装前校验）

```bash
#!/usr/bin/env bash
set -euo pipefail
echo "[1/3] 哈希清单校验"; sha256sum -c checksums.sha256
echo "[2/3] 导入镜像";     docker load -i images/iot-daq-gateway_*.tar
IMG=$(yq '.services.gateway.image' docker-compose.yml)
echo "[3/3] cosign 验签"
cosign verify --key cosign/iot-daq-supply.pub "$IMG" \
  || { echo "FAIL: 验签失败，拒绝安装，请联系厂商"; exit 1; }
echo "OK: 供应链校验通过，可执行 host-fingerprint-collect.sh 后 docker compose up -d"
```

规则：
- `verify.sh` 失败 = **硬失败**，禁止跳过继续安装；失败包整包作废，从厂商渠道重新获取；
- 现场若暂无 cosign 二进制，verify.sh 退化为 `[1/3]+[2/3]` 并打印醒目警告「未完成验签」，由安装人员按手册核对 digest 指纹后放行（残余风险记录于 threat-model.md §3）；
- 包内**敏感物空清单**：无私钥、无授权文件、无客户数据、无 KMS 凭据（与 `container-persistence-layout.md` 口径一致）。

## 8. 私有 registry 可选方案

| 部署形态 | 方案 |
|---------|------|
| 首期默认 | **不依赖 registry**：全部走离线 tar，现场仅 `docker load` |
| 客户有内网 registry（Harbor/私有 ECR 类） | 可选：厂商导出镜像为 OCI 布局，客户自行 push 至其内网 registry；**仍需 digest pin + cosign verify**，registry 只做中转不改变校验链 |
| 厂商侧发布 | 内部 Harbor 承载多架构 manifest 与签名，按版本不可变（保留策略而非覆盖） |

约束：无论何种形态，**现场机器不配置任何外网 registry 拉取**；`docker pull` 仅允许来自客户内网 registry 或完全禁止（tar 模式）。

## 9. 红线（与既有设计对齐）

- 镜像内**不内置**：签名私钥、KMS 凭据、客户标识、试用标记（试用标记属 task 43 持久卷与安装包层）；
- 交付物中禁 `latest` 与可变 tag；基础镜像与业务镜像全部 digest pin；
- 现场默认零外网依赖：构建、验签、安装三段均可在纯离线环境复现；
- 校验失败即停，无「跳过校验继续装」的开关。
