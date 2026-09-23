# iot-daq 设计资产索引（docs/design）

> **定稿口径**：客户端界面实现基准 = `prototype/gateway-v2a-glacier.html`（v2 方案 A），管理后台实现基准 = `prototype/admin-console.html`；授权与容器化设计决策以计划 task 40-44 / 59 的规格章节为准，本文目录中的每份资产均为对应规格章节的图形化/结构化落地，冲突时以规格章节为裁。
>
> **状态**：Wave 0 设计已于 **2026-09-23 经用户确认定稿**，进入 Wave 1+ 开发阶段。SVG 均为自包含矢量图（viewBox 宽 1560），可直接在浏览器/文档中查看。

---

## 1. 信任边界与威胁模型（task 40）

| 资产 | 说明 |
|------|------|
| `auth-architecture.svg` | 授权体系总览：四参与方（网关数据面/本地授权模块/licensing-server/admin-console）、三信任区（本地可信/客户可篡改/厂商可控）、信任流 ①-⑨（指纹→激活→绑定→租约→本地验签→心跳→宽限→降级→停止）、A/B/C 三档验证面板与「防御不住什么」角标 |
| `threat-model.md` | 攻击×防御 10 行矩阵（前提/防御/残余风险）、三条自我披露的极限、A/B/C 档位汇总 |

## 2. 机器码与锚点（task 41）

| 资产 | 说明 |
|------|------|
| `machine-binding.svg` | 激活码 1:1 设备 / 设备 1:N 租约的 ER 关系 + 冲突检测（403 CODE_BOUND_TO_OTHER_DEVICE）、N-of-M 判定仪表盘（N=4/M=5）与三个算例、五锚点算法链（归一化→HMAC→anchor_hashes）、Docker 红线盒 |
| `machine-fingerprint.md` | 五锚点规格表（取用方式/易变性/权重/平台差异）、阈值推导、冲突检测四步决策表（供 task 46 实现） |

## 3. 激活与换机状态机（task 42）

| 资产 | 说明 |
|------|------|
| `activation-state-machine.svg` | 设备状态机（7 态 11 迁移含迁移表）+ 激活码状态机（Issued/Bound/Revoked/Reissued，仅 admin 触发）+ 四条失败路径 + 试用→免费档、吊销即时生效判定盒 |
| `activation-reissue.svg` | 换机五泳道时序（客户/原设备/admin-console/licensing-server/新设备），含 DangerConfirmModal、心跳 403 红路径、reissued_from 审计带 |
| `activation-rules.md` | `/activation`、`/heartbeat` 字段级契约（5 类错误码）、宽限与心跳规则、试用联动、吊销即时语义、换机约束与失败路径 |

## 4. 分发物防篡改（task 43）

| 资产 | 说明 |
|------|------|
| `installer-hardening.svg` | 分发→签名校验→安装→首启→运行时自检→授权判定六阶段卡（各含校验点与失败行为）、试用标记四路冗余盒、时钟回拨对抗盒、Linux 容器专用盒、Tier-1/2/3 定案 |
| `installer-hardening.md` | 11 节：防破解分层（Tier-1 必做/Tier-2 缓做/Tier-3 永不做）、试用标记与回拨对抗细节、与 task 40-44 对齐表 |

## 5. 云端授权服务与总管理后台（task 44）

| 资产 | 说明 |
|------|------|
| `licensing-server-and-admin-console.svg` | 云端拓扑：四条控制通道（激活/心跳携审计区间/verify/审计回执）、API 面、9 表索引、KMS 隔离红线、A/B/C 面板与四条设计红线 |
| `licensing-data-model.md` | 9 表 ER（tenant/device/activation_code/lease/heartbeat/audit_receipt/nonce_cache/signing_key/audit_log）+ 8 条设计决策 |
| `licensing-api.md` | 11 端点契约（设备 4 + 管理端 7）、统一响应壳、审计回执白名单 422 语义、A/B/C 档位表、密钥轮换五步、RBAC 矩阵、防重放双重拒绝 |
| `admin-console-wireframe.md` | 8 页线框（列表/详情时间线/吊销四要素/换机/设备含容器形态/租户档位/审计异常≠破解/RBAC），页级验收要点 |

## 6. 容器化交付（task 59）

| 资产 | 说明 |
|------|------|
| `container-delivery.svg` | 四层拓扑（云端供应链区/宿主/容器运行时/南向设备）+ 容器内锚点禁用红条 + 宿主只读锚点与持久卷盒 + 硬禁条（不挂 docker.sock/不用 --privileged/--device+dialout）+ 供应链四步 |
| `container-machine-binding.md` | 宿主锚点逐条 `--mount ...:ro` 命令、容器内锚点禁用理由表、锚点缺失 HMAC 签名降级、重建/重装两条绑定语义推导 |
| `container-persistence-layout.md` | `/var/lib/iot-daq/` 持久卷树、五类持久物映射、敏感物空清单、两条重建语义结论 |
| `container-supply-chain.md` | buildx 多架构（amd64/arm64）、基础镜像 digest pin、cosign key-pair 签名与三步校验链、SBOM 随包、compose digest 引用、离线 tar 清单 + verify.sh、私有 registry 可选方案 |
| `container-deploy.md` | 串口 `--device`+dialout（非 privileged）、`--network host` 理由与缓解、资源限制/重启策略/日志驱动、离线五步安装流程与健康检查判据、compose 参考片段、不做清单五条 |

## 7. 界面设计与原型（task 63/64 + v2 方案 A）

| 资产 | 说明 |
|------|------|
| `ui-design-system.md` | 设计系统：色彩/字体/间距/组件规范（预存，UI 设计基准） |
| `ui-gateway-console.md` | 客户端网关控制台 UI 规格（预存，与 v2 方案 A 并行有效的功能清单来源） |
| `ui-admin-console.md` | 管理后台 UI 规格（预存；task 44 的 `admin-console-wireframe.md` 为其授权页补充） |
| `prototype/gateway-v2a-glacier.html` | **客户端界面实现基准**（v2 方案 A「冰川」主题，2026-09-23 定稿口径） |
| `prototype/admin-console.html` | **管理后台实现基准**（定稿口径） |
| `prototype/gateway-console.html` | 客户端 v1 原型（历史存档，不作为实现基准） |

---

## 阅读顺序建议

1. 授权体系主线：`auth-architecture.svg` → `threat-model.md` → `machine-binding.svg`/`machine-fingerprint.md` → task 42 三件套 → task 43 两件套 → task 44 四件套；
2. 容器交付支线：`container-delivery.svg` → `container-machine-binding.md` → `container-persistence-layout.md` → `container-supply-chain.md` → `container-deploy.md`；
3. 界面支线：`ui-design-system.md` → 两份 UI 规格 → 对应 prototype 基准页。

## 索引统计

- 设计资产：22 项（根目录 19 份 + prototype/ 3 份）
- 覆盖计划任务：40、41、42、43、44、59、63、64
